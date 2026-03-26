/// Structured metrics export (Prometheus / OpenTelemetry).
///
/// Stub — implementation provided by the private managed service crate.
/// The managed service emits per-agent event rates, batch flush latencies,
/// retry counts, and dead-letter queue depth.
pub trait MetricsReporter: Send + Sync {
    fn record_flush(&self, batch_size: usize, duration_us: u64);
    fn record_retry(&self, agent_id: &str, attempt: u32);
    fn record_dead_letter(&self, agent_id: &str, count: usize);
}
