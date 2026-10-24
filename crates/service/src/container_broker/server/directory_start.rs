//! Prepare one owned set of directory volumes before admitting the main process.

use super::super::{directory as authority, directory_helper as quota};
use super::*;

pub(super) async fn fence_main_removal(
    state: &BrokerState,
    entry: &mut LeaseEntry,
) -> Result<(), PlatformError> {
    entry.fenced = true;
    entry.directory_archives.clear();
    entry.generation_cancel.cancel();
    entry.relay = None;
    if !entry.lease.directory_snapshots.is_empty()
        && !state.journal.contains(&entry.lease.lease_key)?
    {
        state.journal.begin(&entry.lease.lease_key)?;
    }
    remove_helper(state, &entry.lease).await
}

pub(super) async fn before(
    state: &BrokerState,
    entry: &mut LeaseEntry,
    operation: &Operation,
) -> Result<(), PlatformError> {
    if entry.lease.directory_snapshots.is_empty() {
        return Ok(());
    }
    if let Operation::Create(name) = operation
        && !name.ends_with("-proxy")
    {
        begin(state, entry).await
    } else if operation.starts_process()
        && operation
            .identity()
            .is_some_and(|name| !name.ends_with("-proxy"))
        && !entry.directories_ready
    {
        Err(invalid())
    } else {
        Ok(())
    }
}

pub(super) async fn after(
    state: &BrokerState,
    entry: &mut LeaseEntry,
    operation: &Operation,
    status: u16,
) -> Result<(), PlatformError> {
    if entry.lease.directory_snapshots.is_empty() {
        return Ok(());
    }
    if let Operation::Create(name) = operation
        && !name.ends_with("-proxy")
    {
        finish(state, entry, status).await?;
    }
    if let Operation::Delete(name) = operation
        && !name.ends_with("-proxy")
        && matches!(status, 204 | 404)
    {
        if state
            .engine
            .json(
                Method::GET,
                &format!("/containers/{name}/json"),
                Value::Null,
            )
            .await?
            .0
            != 404
        {
            return Err(invalid());
        }
        remove_volumes(state, entry).await?;
    }
    Ok(())
}

pub(super) async fn begin(
    state: &BrokerState,
    entry: &mut LeaseEntry,
) -> Result<(), PlatformError> {
    if entry.lease.directory_snapshots.is_empty() {
        return Ok(());
    }
    let key = &entry.lease.lease_key;
    if entry.directories_ready {
        return verify_existing(state, key, &entry.lease, &entry.startup)
            .await?
            .map(|_| ())
            .ok_or_else(invalid);
    }
    if entry
        .lease
        .directory_snapshots
        .iter()
        .zip(&entry.directory_archives.offsets)
        .any(|(directory, offset)| directory.archive.is_some() && offset.is_none())
    {
        return Err(invalid());
    }
    for path in [
        format!("/containers/{key}/json"),
        format!("/containers/{}/json", quota::name(&entry.lease)),
    ] {
        if state.engine.json(Method::GET, &path, Value::Null).await?.0 != 404 {
            return Err(invalid());
        }
    }
    for position in 0..entry.lease.directory_snapshots.len() {
        let name = authority::volume_name(&entry.lease, position);
        if state
            .engine
            .json(Method::GET, &format!("/volumes/{name}"), Value::Null)
            .await?
            .0
            != 404
        {
            return Err(invalid());
        }
    }
    state.journal.begin(key)?;
    entry.fenced = true;
    for position in 0..entry.lease.directory_snapshots.len() {
        let name = authority::volume_name(&entry.lease, position);
        let (status, volume) = state.engine.json(Method::POST,"/volumes/create",
            json!({"Name":name,"Driver":"local","Labels":authority::labels(&entry.lease,position)?})).await?;
        if status != 201 {
            return Err(invalid());
        }
        authority::verify_volume(&volume, &entry.lease, position)?;
    }
    directory::verify_volumes(state, &entry.lease).await
}

pub(super) async fn finish(
    state: &BrokerState,
    entry: &mut LeaseEntry,
    status: u16,
) -> Result<(), PlatformError> {
    if entry.lease.directory_snapshots.is_empty() || entry.directories_ready {
        return Ok(());
    }
    if status != 201 || !entry.fenced || !state.journal.contains(&entry.lease.lease_key)? {
        return Err(invalid());
    }
    let key = &entry.lease.lease_key;
    let (status, main) = state
        .engine
        .json(Method::GET, &format!("/containers/{key}/json"), Value::Null)
        .await?;
    if status != 200
        || main.pointer("/State/Running").and_then(Value::as_bool) != Some(false)
        || main.pointer("/GraphDriver/Name").and_then(Value::as_str) != Some("overlay2")
    {
        return Err(invalid());
    }
    verify_inspection(state, &main, key, &entry.lease, &entry.startup).await?;
    let upper = main
        .pointer("/GraphDriver/Data/UpperDir")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    validate_upper(state, upper).await?;
    let name = quota::name(&entry.lease);
    let (status, created) = state
        .engine
        .json(
            Method::POST,
            &format!("/containers/create?name={name}"),
            quota::request(&state.config, &entry.lease, upper)?,
        )
        .await?;
    if status != 201 {
        return Err(invalid());
    }
    let expected_id = created
        .get("Id")
        .and_then(Value::as_str)
        .filter(|id| opaque(id))
        .ok_or_else(invalid)?;
    let (id, _) = helper(state, &entry.lease).await?.ok_or_else(invalid)?;
    if id != expected_id {
        return Err(invalid());
    }
    state.engine.verify_helper_volume(&state.config).await?;
    state
        .engine
        .verify_helper_bytes(&id, &state.config.exec_helper_sha256)
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let (status, _) = state
            .engine
            .json(
                Method::POST,
                &format!("/containers/{id}/start"),
                Value::Null,
            )
            .await?;
        if status != 204 {
            return Err(invalid());
        }
        let (status, waited) = state
            .engine
            .json(
                Method::POST,
                &format!("/containers/{id}/wait?condition=not-running"),
                Value::Null,
            )
            .await?;
        if status != 200
            || waited.get("StatusCode").and_then(Value::as_i64) != Some(0)
            || waited.get("Error").is_some_and(|error| !error.is_null())
        {
            return Err(invalid());
        }
        let (actual_id, inspection) = helper(state, &entry.lease).await?.ok_or_else(invalid)?;
        if actual_id != id
            || inspection
                .pointer("/State/Running")
                .and_then(Value::as_bool)
                != Some(false)
            || inspection
                .pointer("/State/ExitCode")
                .and_then(Value::as_i64)
                != Some(0)
            || inspection.pointer("/State/Status").and_then(Value::as_str) != Some("exited")
        {
            return Err(invalid());
        }
        Ok::<(), PlatformError>(())
    })
    .await
    .map_err(|_| invalid())??;
    remove_helper(state, &entry.lease).await?;
    let archive_id = if entry
        .lease
        .directory_snapshots
        .iter()
        .any(|directory| directory.archive.is_some())
    {
        Some(create_archive_view(state, &entry.lease).await?)
    } else {
        None
    };
    for (position, directory) in entry.lease.directory_snapshots.iter().enumerate() {
        let Some(archive) = &directory.archive else {
            continue;
        };
        let offset = entry.directory_archives.offsets[position].ok_or_else(invalid)?;
        let end = offset.checked_add(archive.bytes).ok_or_else(invalid)?;
        let file = entry
            .directory_archives
            .file
            .as_ref()
            .ok_or_else(invalid)?
            .try_clone()
            .await
            .map_err(|_| invalid())?;
        let file =
            directory_archive::rebase(file, offset..end, position, state.config.spool_dir.clone())
                .await?;
        let size = file.metadata().await.map_err(|_| invalid())?.len();
        let id = archive_id.as_ref().ok_or_else(invalid)?;
        let response = state
            .engine
            .send_archive(
                Method::PUT,
                &format!("/containers/{id}/archive?path=%2F"),
                file,
                0..size,
            )
            .await?;
        if response.status().as_u16() != 200 || !bounded_response(response, 0).await?.is_empty() {
            return Err(invalid());
        }
        let (actual_id, inspection) = helper(state, &entry.lease).await?.ok_or_else(invalid)?;
        if actual_id != *id
            || quota::verify_archive(&inspection, &state.config, &entry.lease)? != *id
        {
            return Err(invalid());
        }
    }
    remove_helper(state, &entry.lease).await?;
    let (status, after) = state
        .engine
        .json(Method::GET, &format!("/containers/{key}/json"), Value::Null)
        .await?;
    if status != 200
        || after.get("Id") != main.get("Id")
        || after
            .pointer("/GraphDriver/Data/UpperDir")
            .and_then(Value::as_str)
            != Some(upper)
        || after.pointer("/State/Running").and_then(Value::as_bool) != Some(false)
    {
        return Err(invalid());
    }
    verify_inspection(state, &after, key, &entry.lease, &entry.startup).await?;
    entry.directory_archives.clear();
    state.journal.clear(key)?;
    entry.directories_ready = true;
    entry.fenced = false;
    Ok(())
}

async fn create_archive_view(
    state: &BrokerState,
    lease: &BrokerLease,
) -> Result<String, PlatformError> {
    let name = quota::name(lease);
    let (status, created) = state
        .engine
        .json(
            Method::POST,
            &format!("/containers/create?name={name}"),
            quota::archive_request(&state.config, lease)?,
        )
        .await?;
    if status != 201 {
        return Err(invalid());
    }
    let expected = created
        .get("Id")
        .and_then(Value::as_str)
        .filter(|id| opaque(id))
        .ok_or_else(invalid)?;
    let (id, inspection) = helper(state, lease).await?.ok_or_else(invalid)?;
    if id != expected || quota::verify_archive(&inspection, &state.config, lease)? != expected {
        return Err(invalid());
    }
    Ok(id)
}

async fn validate_upper(state: &BrokerState, upper: &str) -> Result<(), PlatformError> {
    let (status, info) = state.engine.json(Method::GET, "/info", Value::Null).await?;
    if status != 200 || info.get("Driver").and_then(Value::as_str) != Some("overlay2") {
        return Err(invalid());
    }
    quota::upper_path(
        info.get("DockerRootDir")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?,
        upper,
    )
}

pub(super) async fn helper(
    state: &BrokerState,
    lease: &BrokerLease,
) -> Result<Option<(String, Value)>, PlatformError> {
    if lease.directory_snapshots.is_empty() {
        return Ok(None);
    }
    let (status, value) = state
        .engine
        .json(
            Method::GET,
            &format!("/containers/{}/json", quota::name(lease)),
            Value::Null,
        )
        .await?;
    if status == 404 {
        return Ok(None);
    }
    if status != 200 {
        return Err(invalid());
    }
    let id = if value
        .pointer("/Config/Labels/dev.open-compute.container.role")
        .and_then(Value::as_str)
        == Some("directory-archive")
    {
        quota::verify_archive(&value, &state.config, lease)?
    } else {
        let upper = value
            .pointer("/HostConfig/Mounts/1/Source")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?;
        validate_upper(state, upper).await?;
        quota::verify(&value, &state.config, lease, upper)?
    };
    Ok(Some((id, value)))
}

pub(super) async fn remove_helper(
    state: &BrokerState,
    lease: &BrokerLease,
) -> Result<(), PlatformError> {
    let Some((id, _)) = helper(state, lease).await? else {
        return Ok(());
    };
    let name = quota::name(lease);
    let (status, _) = state
        .engine
        .json(
            Method::DELETE,
            &format!("/containers/{id}?force=true"),
            Value::Null,
        )
        .await?;
    if !matches!(status, 204 | 404) {
        return Err(invalid());
    }
    for identity in [id, name] {
        if state
            .engine
            .json(
                Method::GET,
                &format!("/containers/{identity}/json"),
                Value::Null,
            )
            .await?
            .0
            != 404
        {
            return Err(invalid());
        }
    }
    Ok(())
}

pub(super) async fn remove_volumes(
    state: &BrokerState,
    entry: &mut LeaseEntry,
) -> Result<(), PlatformError> {
    for position in 0..entry.lease.directory_snapshots.len() {
        let name = authority::volume_name(&entry.lease, position);
        let (status, volume) = state
            .engine
            .json(Method::GET, &format!("/volumes/{name}"), Value::Null)
            .await?;
        if status == 404 {
            continue;
        }
        if status != 200 {
            return Err(invalid());
        }
        authority::verify_volume(&volume, &entry.lease, position)?;
        let (status, _) = state
            .engine
            .json(Method::DELETE, &format!("/volumes/{name}"), Value::Null)
            .await?;
        if !matches!(status, 204 | 404)
            || state
                .engine
                .json(Method::GET, &format!("/volumes/{name}"), Value::Null)
                .await?
                .0
                != 404
        {
            return Err(invalid());
        }
    }
    entry.directory_archives.clear();
    entry.directories_ready = entry.lease.directory_snapshots.is_empty();
    Ok(())
}
