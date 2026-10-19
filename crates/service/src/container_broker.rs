//! Operator-owned, Unix-only restricted Container engine Broker.
//!
//! The daemon performs preflight and registers persisted leases. It never starts,
//! supervises, or stops this independently operated process or the Docker engine.

mod client;
mod contract;
mod credentials;
mod engine;
mod helper;
mod journal;
mod policy;
mod relay;
mod server;
mod socket;
mod spool;
mod startup;

pub use client::QualifiedContainerProvider;
pub use contract::{BrokerConfig, BrokerLease, ProviderContract, ProviderLeaseObservation};
pub use credentials::BrokerControlCredentials;
pub use startup::{BrokerLabel, BrokerStartOptions};

/// Run the explicitly selected Broker in the foreground without launching an engine.
pub async fn run_broker(
    config: BrokerConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), open_compute_core::PlatformError> {
    server::run(config, shutdown).await
}

#[cfg(test)]
#[path = "container_broker/tests.rs"]
mod tests;
