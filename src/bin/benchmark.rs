/// Integration benchmark — Community Edition vs Managed Edition.
///
/// Run both builds to compare:
///
///   cargo run --bin benchmark                    # Community Edition (limits active)
///   cargo run --bin benchmark --features managed # Managed Edition (no limits)
///
/// Scenarios:
///   1. save_event latency     — NATS PubAck round-trip (same for both editions)
///   2. Batch persistence lag  — time-to-TiDB with community config (batch=64, 100ms)
///                               vs managed config (batch=256, 10ms)
///   3. Rate limit ceiling     — saturate save_event; count accepted vs rejected
///   4. Concurrent throughput  — 500 agents in parallel; measures events/sec
///   5. Baseline MySQL INSERT   — direct single-row INSERT, no NATS (DB-first comparator)
///
/// Prerequisites:
///   docker compose up -d
///   cargo run --bin benchmark
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_nats::jetstream;
use durable_agent_core::durable_event_store::{DurableEventStore, DurableEventStoreConfig};
use durable_agent_core::limits;
use sqlx::mysql::MySqlPoolOptions;
use tokio::sync::Barrier;

fn community_config() -> DurableEventStoreConfig {
    DurableEventStoreConfig {
        max_batch_size: 64,
        max_batch_linger: Duration::from_millis(100),
        channel_capacity: 10_000,
    }
}

fn managed_config() -> DurableEventStoreConfig {
    DurableEventStoreConfig {
        max_batch_size: 256,
        max_batch_linger: Duration::from_millis(10),
        channel_capacity: 10_000,
    }
}

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

async fn bench_save_event_latency(store: &DurableEventStore, iterations: usize) {
    println!("\n━━━ 1. save_event latency ({iterations} iterations) ━━━");
    println!("  Caller-visible cost: NATS JetStream PubAck. TiDB write is async.\n");

    let mut latencies = Vec::with_capacity(iterations);
    for i in 0..iterations {
        let t = Instant::now();
        store
            .save_event(
                "bench-latency",
                &format!("evt-{i}"),
                &serde_json::json!({ "step": i }),
            )
            .await
            .expect("save_event failed");
        latencies.push(t.elapsed().as_micros());
    }
    print_latencies("NATS PubAck", latencies);
}

async fn bench_batch_persistence(
    nats_url: &str,
    tidb_url: &str,
    pool: &sqlx::MySqlPool,
    event_count: usize,
) {
    println!("\n━━━ 2. Batch persistence lag ({event_count} events) ━━━");
    println!("  Time from last publish until all rows are visible in TiDB.");
    println!("  Community: batch=64, linger=100ms   Managed: batch=256, linger=10ms\n");

    struct Profile {
        name: &'static str,
        cfg: DurableEventStoreConfig,
    }

    let profiles = [
        Profile {
            name: "Community  (batch= 64, linger=100ms)",
            cfg: community_config(),
        },
        Profile {
            name: "Managed    (batch=256, linger= 10ms)",
            cfg: managed_config(),
        },
    ];

    for profile in &profiles {
        let store = DurableEventStore::connect(nats_url, tidb_url, profile.cfg.clone())
            .await
            .expect("connect failed");

        let run_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        for i in 0..event_count {
            match store
                .save_event(
                    "bench-batch",
                    &format!("evt-{run_id}-{i}"),
                    &serde_json::json!({ "step": i }),
                )
                .await
            {
                Ok(()) => {}
                Err(e) => panic!("save_event failed: {e}"),
            }
        }

        let start = Instant::now();
        let deadline = Duration::from_secs(15);
        loop {
            let (count,): (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM agent_events WHERE agent_id = 'bench-batch'")
                    .fetch_one(pool)
                    .await
                    .unwrap_or((0,));

            if count >= event_count as i64 {
                break;
            }
            if start.elapsed() > deadline {
                println!(
                    "  {}  TIMEOUT ({count}/{event_count} persisted)",
                    profile.name
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        println!("  {}  {:>8.2?}", profile.name, start.elapsed());

        sqlx::query("DELETE FROM agent_events WHERE agent_id = 'bench-batch'")
            .execute(pool)
            .await
            .ok();
    }

    if !limits::is_managed() {
        println!(
            "\n  ↑ Managed config was clamped to Community limits in this build.");
        println!(
            "    Run --features managed to observe the uncapped managed profile.");
    }
}

async fn bench_backpressure(store: Arc<DurableEventStore>, concurrency: usize, events_per_task: usize) {
    let total = concurrency * events_per_task;
    println!("\n━━━ 3. Backpressure under load ({concurrency} tasks × {events_per_task} events = {total} total) ━━━");
    println!("  Community: tasks block at 1,000/sec. Zero events lost — just throttled.");
    println!("  Managed:   no rate limit; bounded only by NATS throughput.\n");

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
                            &format!("bench-backpressure-{task_id}"),
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

    println!("  Edition    : {}", if limits::is_managed() { "Managed" } else { "Community" });
    println!("  Events     : {total} / {total} accepted (zero loss)");
    println!("  Elapsed    : {:>8.2?}", elapsed);
    println!("  Throughput : {:>8.0} events/sec", total as f64 / elapsed.as_secs_f64());
}

async fn bench_concurrent_throughput(
    store: Arc<DurableEventStore>,
    concurrency: usize,
    events_per_task: usize,
) {
    let total = concurrency * events_per_task;
    println!("\n━━━ 4. Concurrent throughput ({concurrency} agents × {events_per_task} events = {total} total) ━━━\n");

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
                            &format!("bench-concurrent-{task_id}"),
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

    println!("  elapsed    {:>8.2?}", elapsed);
    println!("  accepted   {total}/{total} events (zero loss)");
    println!("  throughput {:>8.0} events/sec", total as f64 / elapsed.as_secs_f64());
}

async fn bench_baseline_mysql_insert(pool: &sqlx::MySqlPool, iterations: usize) {
    println!("\n━━━ 5. Baseline: direct MySQL INSERT ({iterations} iterations) ━━━");
    println!("  Single-row synchronous INSERT, no NATS. Represents the per-event cost of");
    println!("  DB-first checkpointers (LangGraph-Postgres, DBOS, vanilla SQLite).\n");

    let mut latencies = Vec::with_capacity(iterations);
    for i in 0..iterations {
        let t = Instant::now();
        sqlx::query(
            "INSERT INTO agent_events (agent_id, event_id, payload_json) VALUES (?, ?, ?)",
        )
        .bind("bench-baseline")
        .bind(format!("evt-{i}"))
        .bind(serde_json::json!({ "step": i }).to_string())
        .execute(pool)
        .await
        .expect("INSERT failed");
        latencies.push(t.elapsed().as_micros());
    }
    print_latencies("MySQL INSERT", latencies);
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();

    let nats_url =
        std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into());
    let tidb_url = std::env::var("TIDB_URL")
        .unwrap_or_else(|_| "mysql://root:root@127.0.0.1:3306/durable_agent".into());

    let edition = if limits::is_managed() {
        "Managed Edition"
    } else {
        "Community Edition"
    };

    println!("durable_agent_core benchmark — {edition}");
    println!("  NATS : {nats_url}");
    println!("  DB   : {tidb_url}");
    if !limits::is_managed() {
        println!(
            "  Caps : {}/sec  batch={}  linger={}ms",
            limits::COMMUNITY_RATE_LIMIT,
            limits::MAX_BATCH_SIZE,
            limits::MIN_FLUSH_INTERVAL_MS,
        );
    }

    let nats = async_nats::connect(&nats_url).await?;
    let js = jetstream::new(nats);
    js.get_or_create_stream(jetstream::stream::Config {
        name: "agent-traces".to_string(),
        subjects: vec!["agent.trace.*".to_string()],
        ..Default::default()
    })
    .await?;

    let pool = MySqlPoolOptions::new()
        .max_connections(5)
        .connect(&tidb_url)
        .await?;

    let store = Arc::new(
        DurableEventStore::connect(&nats_url, &tidb_url, DurableEventStoreConfig::default())
            .await?,
    );

    clear_bench_data(&pool).await;

    bench_save_event_latency(&store, 500).await;
    bench_batch_persistence(&nats_url, &tidb_url, &pool, 1_000).await;
    bench_backpressure(store.clone(), 10, 2_000).await;
    bench_concurrent_throughput(store.clone(), 500, 20).await;
    bench_baseline_mysql_insert(&pool, 500).await;

    println!("\n━━━ Summary ━━━");
    println!("  Scenario 1 p50 vs Scenario 5 p50: NATS-first latency advantage over DB-first.");
    println!("  Scenario 2: managed config (batch=256, 10ms) vs community (batch=64, 100ms).");
    if !limits::is_managed() {
        println!("  Scenario 3: Community capped at {}/sec. Run --features managed to remove cap.",
            limits::COMMUNITY_RATE_LIMIT);
    }
    println!("\n  To compare editions:");
    println!("    cargo run --bin benchmark");
    println!("    cargo run --bin benchmark --features managed");

    clear_bench_data(&pool).await;
    Ok(())
}
