/// Data retention and purging policy.
///
/// Community Edition ships no retention logic. Data in `agent_events` and
/// `agent_traces` accumulates indefinitely. At scale this becomes a storage
/// cost forcing an upgrade conversation.
///
/// The managed service provides:
///   - Configurable TTL per agent (e.g. retain last 30 days of traces)
///   - Automatic archival to cold storage before deletion
///   - Per-tier event count limits (e.g. keep last 10,000 steps)
///   - A purge API for on-demand cleanup
///   - Scheduled background jobs driven by agent activity patterns
///
/// Stub — concrete implementation is in the private managed service crate.
pub trait RetentionPolicy: Send + Sync {
    /// Maximum age of events to retain. None = keep forever.
    fn max_age_days(&self) -> Option<u32>;

    /// Maximum number of trace rows to retain per agent. None = unlimited.
    fn max_events_per_agent(&self) -> Option<usize>;

    /// Apply the retention policy for a given agent, deleting or archiving
    /// rows that exceed the configured limits.
    fn apply<'a>(
        &'a self,
        pool: &'a sqlx::MySqlPool,
        agent_id: &'a str,
    ) -> impl std::future::Future<Output = Result<usize, sqlx::Error>> + Send + 'a;
}
