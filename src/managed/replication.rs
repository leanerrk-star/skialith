/// Multi-region JetStream replication.
///
/// Stub — implementation provided by the private managed service crate.
/// The managed service mirrors each JetStream subject to secondary regions
/// with configurable consistency levels (sync / async).
pub trait RegionReplicator: Send + Sync {
    fn regions(&self) -> &[String];
}
