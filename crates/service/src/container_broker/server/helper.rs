//! Prove the read-only helper input before execution, including create-time replacement races.

use super::*;

pub(super) async fn before(
    state: &BrokerState,
    operation: &Operation,
    existing: Option<&(String, bool)>,
) -> Result<(), PlatformError> {
    if operation.starts_process() {
        state.engine.verify_helper_volume(&state.config).await?;
    }
    if let Operation::Start(name) = operation
        && !name.ends_with("-proxy")
        && let Some((id, _)) = existing
    {
        state
            .engine
            .verify_helper_bytes(id, &state.config.exec_helper_sha256)
            .await?;
    }
    Ok(())
}

pub(super) async fn created(
    state: &BrokerState,
    operation: &Operation,
    status: u16,
    entry: Option<&mut LeaseEntry>,
) -> Result<(), PlatformError> {
    let Operation::Create(name) = operation else {
        return Ok(());
    };
    if status != 201 {
        return Ok(());
    }
    let entry = entry.ok_or_else(invalid)?;
    if name.ends_with("-proxy") {
        let credentials = entry.credentials.clone().ok_or_else(invalid)?;
        state.journal.begin(&entry.lease.lease_key)?;
        entry.fenced = true;
        super::super::credentials::install(&state.engine, name, &credentials).await?;
        state.journal.clear(&entry.lease.lease_key)?;
        entry.fenced = false;
        return Ok(());
    }
    let (id, _) = verify_existing(state, name, &entry.lease, &entry.startup)
        .await?
        .ok_or_else(invalid)?;
    state.engine.verify_helper_volume(&state.config).await?;
    state
        .engine
        .verify_helper_bytes(&id, &state.config.exec_helper_sha256)
        .await
}
