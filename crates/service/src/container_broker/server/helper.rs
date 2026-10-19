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
    lease: Option<&BrokerLease>,
    startup: Option<&super::super::startup::EffectiveStartup>,
) -> Result<(), PlatformError> {
    let Operation::Create(name) = operation else {
        return Ok(());
    };
    if status != 201 || name.ends_with("-proxy") {
        return Ok(());
    }
    let (id, _) = verify_existing(
        state,
        name,
        lease.ok_or_else(invalid)?,
        startup.ok_or_else(invalid)?,
    )
    .await?
    .ok_or_else(invalid)?;
    state.engine.verify_helper_volume(&state.config).await?;
    state
        .engine
        .verify_helper_bytes(&id, &state.config.exec_helper_sha256)
        .await
}
