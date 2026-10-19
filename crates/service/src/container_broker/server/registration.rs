//! Adopt frozen execution policy for admission or exclusively fenced cleanup.

use super::*;

pub(super) async fn register(
    state: &BrokerState,
    lease: BrokerLease,
    cleanup: bool,
) -> Result<Response<Body>, PlatformError> {
    lease.validate(&state.contract)?;
    let (status, image) = state
        .engine
        .json(
            Method::GET,
            &format!("/images/{}/json", lease.image),
            Value::Null,
        )
        .await?;
    if status != 200
        || !policy::image_matches(
            &image,
            &lease.image,
            &lease.diff_ids,
            &state.contract.architecture,
        )
    {
        return Err(invalid());
    }
    let startup = super::super::startup::EffectiveStartup::from_image(&image, &lease)?;
    let mut leases = state.leases.lock().await;
    if let Some(existing) = leases.get(&lease.lease_key) {
        let mut existing = existing.lock().await;
        let mut immutable = lease.clone();
        immutable.max_instances = existing.lease.max_instances;
        if (!cleanup && existing.fenced) || existing.lease != immutable {
            return Err(invalid());
        }
        existing.lease.max_instances = lease.max_instances;
        if cleanup {
            fence(&mut existing);
        }
        return response(200, Vec::new());
    }
    if leases.len() >= state.config.max_leases as usize {
        return Err(invalid());
    }
    let mut live = state.unknown.lock().await.len();
    let mut application = 0;
    for entry in leases.values() {
        let entry = entry.lock().await;
        if !entry.cleaned {
            live += 1;
            if entry.lease.application_id == lease.application_id {
                application += 1;
            }
        }
    }
    let recovered = state.unknown.lock().await.contains(&lease.lease_key);
    if !cleanup
        && ((!recovered && live >= state.config.max_instances as usize)
            || application >= lease.max_instances as usize)
    {
        return Err(invalid());
    }
    for name in [
        lease.lease_key.clone(),
        format!("{}-proxy", lease.lease_key),
    ] {
        verify_existing(state, &name, &lease, &startup).await?;
    }
    let fenced = cleanup || state.journal.contains(&lease.lease_key)?;
    state.unknown.lock().await.remove(&lease.lease_key);
    leases.insert(
        lease.lease_key.clone(),
        Arc::new(Mutex::new(LeaseEntry {
            fenced,
            lease,
            startup,
            cleaned: false,
            process_identity: None,
            relay: None,
            credentials: None,
            generation_sha256: None,
            generation_cancel: state.shutdown.child_token(),
        })),
    );
    response(201, Vec::new())
}

fn fence(entry: &mut LeaseEntry) {
    entry.fenced = true;
    entry.generation_cancel.cancel();
    entry.relay = None;
    entry.credentials = None;
    entry.generation_sha256 = None;
}
