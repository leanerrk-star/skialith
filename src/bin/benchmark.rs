/// Integration benchmark for durable_agent_core.
///
/// Scenarios:
///   1. save_event latency     — p50/p95/p99 for a single agent publishing sequentially.
///                               This is dominated by the NATS JetStream PubAck round-trip.
///   2. Concurrent throughput  — N agents publishing in parallel; measures events/sec.
///   3. Batch size comparison  — same load replayed with batch sizes 64 / 256 / 512;
///                               measures time-to-persistence in TiDB.
///   4. Baseline comparison    — direct single-row MySQL INSERT with no NATS, no batching.
///                               This is what Postgres-based checkpointers (LangGraph, DBOS)
///                               do per event. Comparing (1) vs (4) proves the NATS-first thesis.
///
/// Prerequisites:
///   docker compose up -d
///   # Wait for MySQL and NATS to be healthy, then:
///   cargo run --bin benchmark
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_nats::jetstream;
use durable_agent_core::durable_event_store::{DurableEventStore, DurableEventStoreConfig};
use sqlx::mysql::MySqlPoolOptions;
use tokio::sync::Barrier;

// ── helpers ──────────────────────────────────────────────────────────────────

fn percentile(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64) * p / 100.0) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn print_latencies(label: &str, mut latencies: Vec<u128>) {
    latencies.sort_unstable();
    println!("  {label}");
    println!("    p50  {:>7}µs", percentile(&latencies, 50.0));
    println!("    p95  {:>7}µs", percentile(&latencies, 95.0));
    println!("    p99  {:>7}µs", percentile(&latencies, 99.0));
    println!("    max  {:>7}µs", latencies.last().copied().unwrap_or(0));
}

async fn clear_bench_data(pool: &sqlx::MySqlPool) {
    sqlx::query("DELETE FROM agent_events WHERE agent_id LIKE 'bench-%'")
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM agent_traces WHERE agent_id LIKE 'bench-%'")
        .execute(pool)
        .await
        .ok();
}

// ── scenario 1: save_event latency ───────────────────────────────────────────

async fn bench_save_event_latency(store: &DurableEventStore, iterations: usize) {
    println!("\n━━━ Scenario 1: save_event latency ({iterations} iterations) ━━━");
    println!("  What: time for one save_event call to return (= NATS PubAck round-trip).");
    println!("  The TiDB write happens in the background — this is the caller-visible cost.\n");

    let mut latencies = Vec::with_capacity(iterations);

    for i in 0..iterations {
        let start = Instant::now();
        store
            .save_event(
                "bench-agent-single",
                &format!("evt-{i}"),
                &serde_json::json!({ "step": i, "thought": "benchmark payload" }),
            )
            .await
            .expect("save_event failed");
        latencies.push(start.elapsed().as_micros());
    }

    print_latencies("save_event (NATS PubAck)", latencies);
}

// ── scenario 2: concurrent throughput ────────────────────────────────────────

async fn bench_concurrent_throughput(
    store: Arc<DurableEventStore>,
    concurrency: usize,
    events_per_task: usize,
) {
    let total = concurrency * events_per_task;
    println!("\n━━━ Scenario 2: concurrent throughput ━━━");
    println!("  concurrency={concurrency}, events_per_task={events_per_task}, total={total}\n");

    // Barrier ensures all tasks start simultaneously for a fair measurement.
    let barrier = Arc::new(Barrier::new(concurrency));

    let start = Instant::now();

    let handles: Vec<_> = (0..concurrency)
        .map(|task_id| {
            let store = store.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                for i in 0..events_per_task {
                    store
                        .save_event(
                            &format!("bench-agent-{task_id}"),
                            &format!("evt-{i}"),
                            &serde_json::json!({ "task": task_id, "step": i }),
                        )
                        .await
                        .expect("save_event failed");
                }
            })
        })
        .collect();

    futures::future::join_all(handles).await;
    let elapsed = start.elapsed();
    let throughput = total as f64 / elapsed.as_secs_f64();

    println!("  elapsed    {:>8.2?}", elapsed);
    println!("  throughput {:>8.0} events/sec", throughput);
}

// ── scenario 3: batch size comparison ────────────────────────────────────────

async fn bench_batch_sizes(
    nats_url: &str,
    tidb_url: &str,
    event_count: usize,
) {
    println!("\n━━━ Scenario 3: batch size comparison ({event_count} events) ━━━");
    println!("  What: time until all events are visible in TiDB (persistence lag).");
    println!("  Smaller batches = lower lag but more DB round-trips.\n");

    for batch_size in [64usize, 256, 512] {
        let cfg = DurableEventStoreConfig {
            max_batch_size: batch_size,
            max_batch_linger: Duration::from_millis(50),
            channel_capacity: 10_000,
        };
        let store = DurableEventStore::connect(nats_url, tidb_url, cfg)
            .await
            .expect("connect failed");

        // Publish all events as fast as possible.
        for i in 0..event_count {
            store
                .save_event(
                    "bench-agent-batch",
                    &format!("evt-batch{batch_size}-{i}"),
                    &serde_json::json!({ "step": i }),
                )
                .await
                .expect("save_event failed");
        }

        // Poll TiDB until the expected row count is reached (or 10 s timeout).
        let pool = store.tidb_pool().clone();
        let start = Instant::now();
        let deadline = Duration::from_secs(10);
        loop {
            let (count,): (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM agent_events WHERE agent_id = 'bench-agent-batch'",
            )
            .fetch_one(&pool)
            .await
            .unwrap_or((0,));

            if count >= event_count as i64 {
                break;
            }
            if start.elapsed() > deadline {
                println!("  batch_size={batch_size:>4}  TIMEOUT (only {count}/{event_count} persisted)");
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        println!(
            "  batch_size={batch_size:>4}  time-to-persistence {:>8.2?}",
            start.elapsed()
        );

        // Clean up between runs so counts don't bleed across batch sizes.
        sqlx::query("DELETE FROM agent_events WHERE agent_id = 'bench-agent-batch'")
            .execute(store.tidb_pool())
            .await
            .ok();
    }
}

// ── scenario 4: baseline — direct MySQL INSERT (no NATS, no batching) ────────

async fn bench_baseline_mysql_insert(pool: &sqlx::MySqlPool, iterations: usize) {
    println!("\n━━━ Scenario 4: baseline — direct MySQL INSERT ({iterations} iterations) ━━━");
    println!("  What: single-row synchronous INSERT, no NATS.");
    println!("  This is the per-event cost of Postgres-based checkpointers");
    println!("  (LangGraph-Postgres, DBOS) that block the caller on every write.\n");

    let mut latencies = Vec::with_capacity(iterations);

    for i in 0..iterations {
        let start = Instant::now();
        sqlx::query(
            "INSERT INTO agent_events (agent_id, event_id, payload_json) VALUES (?, ?, ?)",
        )
        .bind("bench-agent-baseline")
        .bind(format!("evt-baseline-{i}"))
        .bind(serde_json::json!({ "step": i }).to_string())
        .execute(pool)
        .await
        .expect("INSERT failed");
        latencies.push(start.elapsed().as_micros());
    }

    print_latencies("direct MySQL INSERT", latencies);

    println!();
    println!("  ↑ Compare this p50 to Scenario 1 p50.");
    println!("  The gap is the latency advantage of NATS-first over DB-first.");
}

// ── main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();

    let nats_url =
        std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into());
    let tidb_url = std::env::var("TIDB_URL")
        .unwrap_or_else(|_| "mysql://root:root@127.0.0.1:3306/durable_agent".into());

    println!("durable_agent_core benchmark");
    println!("  NATS : {nats_url}");
    println!("  DB   : {tidb_url}");

    // Ensure the JetStream stream exists before publishing.
    let nats = async_nats::connect(&nats_url).await?;
    let js = jetstream::new(nats);
    js.get_or_create_stream(jetstream::stream::Config {
        name: "agent-traces".to_string(),
        subjects: vec!["agent.trace.*".to_string()],
        ..Default::default()
    })
    .await?;

    // Standalone pool for the baseline scenario.
    let pool = MySqlPoolOptions::new()
        .max_connections(5)
        .connect(&tidb_url)
        .await?;

    // Default store used for scenarios 1 & 2.
    let store = Arc::new(
        DurableEventStore::connect(&nats_url, &tidb_url, DurableEventStoreConfig::default())
            .await?,
    );

    clear_bench_data(&pool).await;

    // ── run scenarios ──────────────────────────────────────────────────────
    bench_save_event_latency(&store, 500).await;
    bench_concurrent_throughput(store.clone(), 100, 20).await;
    bench_concurrent_throughput(store.clone(), 500, 20).await;
    bench_batch_sizes(&nats_url, &tidb_url, 1_000).await;
    bench_baseline_mysql_insert(&pool, 500).await;

    // ── summary ────────────────────────────────────────────────────────────
    println!("\n━━━ Done ━━━");
    println!("  Key comparison: Scenario 1 p50 (NATS PubAck) vs Scenario 4 p50 (MySQL INSERT).");
    println!("  A lower Scenario 1 p50 proves the NATS-first architecture reduces caller latency.");

    clear_bench_data(&pool).await;
    Ok(())
}
