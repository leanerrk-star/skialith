/// Managed Edition extension points.
///
/// This module is compiled only when `--features managed` is active.
/// The OSS repository ships stub declarations here; the full implementations
/// are provided by the private managed service crate that depends on this one.
///
/// Features gated behind this module (not available in Community Edition):
/// - Multi-region JetStream replication
/// - Sub-10ms flush intervals
/// - Unlimited concurrent agents and events/sec
/// - Priority message routing (X-WAL-Phase headers)
/// - Structured metrics export (Prometheus / OpenTelemetry)
/// - Admin API for live config reload
#[cfg(feature = "managed")]
pub mod replication;

#[cfg(feature = "managed")]
pub mod metrics;
