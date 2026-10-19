//! Fixed-Wrangler Durable Object export/migration lifecycle adapter.

use super::errors::{invalid, unsupported};
use super::model::{WorkerUploadExport, WorkerUploadMetadata};
use crate::workers_http::WorkerApiState;
use open_compute_core::{InstanceId, PlatformError, WorkerId};
use open_compute_storage::durable_objects::{
    DurableObjectClassRename, DurableObjectMigrationHead, DurableObjectMigrationPlan,
    DurableObjectMigrationPreparation, DurableObjectRepository,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct PreparedDoMigration {
    plan: DurableObjectMigrationPlan,
}

impl PreparedDoMigration {
    pub(super) fn tag(&self) -> &str {
        &self.plan.new_tag
    }

    pub(super) fn plan(&self) -> &DurableObjectMigrationPlan {
        &self.plan
    }

    pub(super) fn rollback(
        &self,
        api: &WorkerApiState,
        worker_id: WorkerId,
        now_ms: i64,
    ) -> Result<(), PlatformError> {
        DurableObjectRepository::new(&api.storage).rollback_worker_migration(
            worker_id,
            &self.plan.new_tag,
            now_ms,
        )
    }
}

pub(super) fn prepare(
    api: &WorkerApiState,
    instance_id: InstanceId,
    worker_id: WorkerId,
    metadata: &WorkerUploadMetadata,
    bundle: Option<&[u8]>,
    now_ms: i64,
) -> Result<Option<PreparedDoMigration>, PlatformError> {
    validate_containers(metadata)?;
    let repository = DurableObjectRepository::new(&api.storage);
    let current = repository.current_worker_migration(worker_id)?;
    let current_tag = current.as_ref().map(|migration| migration.tag.clone());
    let Some(mut plan) = migration_plan(metadata, current_tag, bundle)? else {
        return Ok(None);
    };
    normalize_declarative_replay_base(&mut plan, current.as_ref());
    plan.new_sqlite_classes.sort();
    let preparation = repository.prepare_worker_migration(instance_id, worker_id, &plan, now_ms)?;
    // A repeated cf export declaration describes the existing class authority.
    // New Versions may change vars/secrets without claiming its committed migration.
    if plan.declarative
        && matches!(
            preparation,
            DurableObjectMigrationPreparation::AlreadyCommitted { .. }
        )
    {
        return Ok(None);
    }
    Ok(Some(PreparedDoMigration { plan }))
}

fn normalize_declarative_replay_base(
    plan: &mut DurableObjectMigrationPlan,
    current: Option<&DurableObjectMigrationHead>,
) {
    if plan.declarative
        && let Some(current) = current
        && current.tag == plan.new_tag
    {
        plan.old_tag.clone_from(&current.old_tag);
    }
}

pub(super) fn declares_live_class(metadata: &WorkerUploadMetadata, class_name: &str) -> bool {
    if metadata.migrations.as_ref().is_some_and(|migrations| {
        migrations.steps.iter().any(|step| {
            step.new_sqlite_classes
                .iter()
                .any(|name| name == class_name)
                || step
                    .renamed_classes
                    .iter()
                    .any(|rename| rename.to == class_name)
        })
    }) {
        return true;
    }
    metadata
        .exports
        .as_ref()
        .and_then(|exports| exports.get(class_name))
        .is_some_and(|export| match export {
            WorkerUploadExport::DurableObject { state, storage, .. } => {
                state.as_deref().unwrap_or("created") == "created"
                    && storage.as_deref() == Some("sqlite")
            }
            WorkerUploadExport::Worker { .. } | WorkerUploadExport::Workflow { .. } => false,
        })
}

pub(super) fn validate_containers(metadata: &WorkerUploadMetadata) -> Result<(), PlatformError> {
    let mut classes = BTreeSet::new();
    let mut names = BTreeSet::new();
    for container in &metadata.containers {
        open_compute_storage::containers::validate_container_images(&container.images).map_err(
            |_| invalid("Container images require valid names and immutable references"),
        )?;
        if container.name.is_empty()
            || container.name.len() > 63
            || !container
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || !classes.insert(&container.class_name)
            || !names.insert(&container.name)
        {
            return Err(invalid(
                "Container declarations must have unique valid names and classes",
            ));
        }
        let export = metadata
            .exports
            .as_ref()
            .and_then(|exports| exports.get(&container.class_name));
        if !matches!(export, Some(WorkerUploadExport::DurableObject { state, storage, container: Some(name), renamed_to: None, transferred_to: None, transfer_from: None }) if state.as_deref().unwrap_or("created") == "created" && storage.as_deref() == Some("sqlite") && name == &container.name)
        {
            return Err(invalid(
                "Container declarations require a matching same-Worker SQLite export",
            ));
        }
    }
    if metadata.exports.as_ref().is_some_and(|exports| {
        exports.iter().any(|(class, export)| {
            matches!(
                export,
                WorkerUploadExport::DurableObject {
                    container: Some(_),
                    ..
                }
            ) && !classes.contains(class)
        })
    }) {
        return Err(invalid(
            "Container exports require an application declaration",
        ));
    }
    Ok(())
}

fn migration_plan(
    metadata: &WorkerUploadMetadata,
    current_tag: Option<String>,
    bundle: Option<&[u8]>,
) -> Result<Option<DurableObjectMigrationPlan>, PlatformError> {
    let durable_exports = metadata
        .exports
        .as_ref()
        .map(|exports| {
            exports
                .iter()
                .filter(|(_, export)| matches!(export, WorkerUploadExport::DurableObject { .. }))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    if metadata.migrations.is_some() && !durable_exports.is_empty() {
        return Err(invalid(
            "Durable Object exports and migrations are mutually exclusive",
        ));
    }
    if let Some(migrations) = &metadata.migrations {
        if migrations.steps.is_empty() {
            return Err(invalid("Durable Object migration steps are empty"));
        }
        let mut plan = DurableObjectMigrationPlan {
            declarative: false,
            old_tag: migrations.old_tag.clone(),
            new_tag: migrations.new_tag.clone(),
            new_sqlite_classes: Vec::new(),
            renamed_classes: Vec::new(),
            deleted_classes: Vec::new(),
        };
        for step in &migrations.steps {
            if !step.new_classes.is_empty() || !step.transferred_classes.is_empty() {
                return Err(unsupported(
                    "only SQLite Durable Object migrations are supported",
                ));
            }
            plan.new_sqlite_classes
                .extend(step.new_sqlite_classes.iter().cloned());
            plan.renamed_classes
                .extend(
                    step.renamed_classes
                        .iter()
                        .map(|rename| DurableObjectClassRename {
                            from: rename.from.clone(),
                            to: rename.to.clone(),
                        }),
                );
            plan.deleted_classes
                .extend(step.deleted_classes.iter().cloned());
        }
        return Ok(Some(plan));
    }
    if durable_exports.is_empty() {
        return Ok(None);
    }
    let bundle = bundle.ok_or_else(|| invalid("Durable Object exports require Worker code"))?;
    declarative_export_plan(&durable_exports, current_tag, Sha256::digest(bundle).into()).map(Some)
}

fn declarative_export_plan(
    exports: &BTreeMap<&String, &WorkerUploadExport>,
    current_tag: Option<String>,
    bundle_sha256: [u8; 32],
) -> Result<DurableObjectMigrationPlan, PlatformError> {
    let mut live = BTreeSet::new();
    let mut rename_targets = BTreeSet::new();
    let mut renamed_classes = Vec::new();
    let mut deleted_classes = Vec::new();
    let mut tag_material = bundle_sha256.to_vec();
    for (name, export) in exports {
        let WorkerUploadExport::DurableObject {
            state,
            storage,
            renamed_to,
            container,
            transferred_to,
            transfer_from,
        } = export
        else {
            continue;
        };
        tag_material.extend_from_slice(name.as_bytes());
        tag_material.push(0);
        tag_material.extend_from_slice(state.as_deref().unwrap_or("created").as_bytes());
        tag_material.push(0);
        tag_material.extend_from_slice(storage.as_deref().unwrap_or_default().as_bytes());
        tag_material.push(0);
        tag_material.extend_from_slice(renamed_to.as_deref().unwrap_or_default().as_bytes());
        tag_material.push(0xff);
        tag_material.extend_from_slice(container.as_deref().unwrap_or_default().as_bytes());
        tag_material.push(0);
        if transferred_to.is_some() || transfer_from.is_some() {
            return Err(unsupported(
                "Durable Object cross-Script transfer is unsupported",
            ));
        }
        match state.as_deref().unwrap_or("created") {
            "created" if storage.as_deref() == Some("sqlite") && renamed_to.is_none() => {
                live.insert((*name).clone());
            }
            "deleted" if storage.is_none() && renamed_to.is_none() => {
                deleted_classes.push((*name).clone());
            }
            "renamed" if storage.is_none() => {
                let target = renamed_to
                    .as_ref()
                    .ok_or_else(|| invalid("Durable Object rename target is missing"))?;
                rename_targets.insert(target.clone());
                renamed_classes.push(DurableObjectClassRename {
                    from: (*name).clone(),
                    to: target.clone(),
                });
            }
            _ => {
                return Err(unsupported(
                    "Durable Object export lifecycle shape is unsupported",
                ));
            }
        }
    }
    if !rename_targets.is_subset(&live) {
        return Err(invalid(
            "Durable Object rename targets must also be live SQLite exports",
        ));
    }
    let new_sqlite_classes = live
        .difference(&rename_targets)
        .cloned()
        .collect::<Vec<_>>();
    let digest = Sha256::digest(&tag_material);
    Ok(DurableObjectMigrationPlan {
        declarative: true,
        old_tag: current_tag,
        new_tag: format!("exports-{}", &hex::encode(digest)[..32]),
        new_sqlite_classes,
        renamed_classes,
        deleted_classes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn repeated_cf_exports_do_not_claim_the_original_versions_migration() {
        use open_compute_core::{RequestId, VersionId};
        use open_compute_storage::worker_repository::{
            EffectiveResourceLimits, NewVersion, NewVersionProducts, VersionContentKind,
            WorkerRepository,
        };

        let (_temp, _mock, state, account, _storage) =
            crate::tests::initialized_worker_http_fixture().await;
        let api = state.worker_api().unwrap();
        let workers = WorkerRepository::new(api.storage.db());
        let (worker, _) = workers
            .create_worker(account, "repeated-export", RequestId::generate(), 1, 100)
            .unwrap();
        let declaration = metadata(serde_json::json!({
            "main_module": "index.js",
            "compatibility_date": "2026-09-08",
            "exports": {"Counter": {"type": "durable-object", "storage": "sqlite"}}
        }));
        let migration = prepare(api, account, worker.id, &declaration, Some(b"code"), 2)
            .unwrap()
            .unwrap();
        let version = VersionId::generate();
        workers
            .insert_staging_version(
                &NewVersion {
                    id: version,
                    instance_id: account,
                    worker_id: worker.id,
                    content_kind: VersionContentKind::Worker,
                    artifact_sha256: Some([7; 32]),
                    artifact_size: Some(1),
                    artifact_schema_version: Some(1),
                    main_module: Some("index.js".to_owned()),
                    worker_code_sha256: [8; 32],
                    compatibility_date: "2026-09-08".to_owned(),
                    compatibility_flags: Vec::new(),
                    resource_limits: EffectiveResourceLimits::standard_defaults(),
                    vars: BTreeMap::new(),
                    secrets: BTreeMap::new(),
                    request_id: RequestId::generate(),
                    now_ms: 3,
                },
                &NewVersionProducts::default(),
                100,
            )
            .unwrap();
        workers.begin_validation(version).unwrap();
        workers
            .mark_ready_with_durable_object_migration(version, worker.id, migration.plan(), 4)
            .unwrap();
        let repository = DurableObjectRepository::new(&api.storage);
        let original = repository
            .current_worker_migration(worker.id)
            .unwrap()
            .unwrap();
        assert!(
            prepare(api, account, worker.id, &declaration, Some(b"code"), 5)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            repository
                .current_worker_migration(worker.id)
                .unwrap()
                .unwrap()
                .tag,
            original.tag
        );
        assert_eq!(
            repository
                .current_worker_migration(worker.id)
                .unwrap()
                .unwrap()
                .version_id,
            version
        );

        let conflicting = metadata(serde_json::json!({
            "main_module": "index.js",
            "compatibility_date": "2026-09-08",
            "migrations": {"new_tag": original.tag, "steps": [{"new_sqlite_classes": ["Counter"]}]}
        }));
        assert_eq!(
            prepare(api, account, worker.id, &conflicting, Some(b"code"), 6)
                .err()
                .unwrap()
                .code(),
            open_compute_core::ErrorCode::IdempotencyConflict
        );
        let changed = prepare(
            api,
            account,
            worker.id,
            &declaration,
            Some(b"changed code"),
            7,
        )
        .unwrap()
        .unwrap();
        assert_ne!(changed.tag(), original.tag);
        changed.rollback(api, worker.id, 8).unwrap();
        assert_eq!(
            repository
                .current_worker_migration(worker.id)
                .unwrap()
                .unwrap()
                .version_id,
            version
        );
    }

    fn metadata(value: serde_json::Value) -> WorkerUploadMetadata {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn fixed_wrangler_migrations_and_exports_map_to_one_sqlite_plan() {
        let migrations = metadata(serde_json::json!({
            "main_module": "index.js",
            "compatibility_date": "2026-09-08",
            "migrations": {
                "old_tag": "v1",
                "new_tag": "v2",
                "steps": [{
                    "new_sqlite_classes": ["Created"],
                    "renamed_classes": [{"from": "Old", "to": "Renamed"}],
                    "deleted_classes": ["Deleted"]
                }]
            }
        }));
        let plan = migration_plan(&migrations, Some("v1".to_owned()), Some(b"first code"))
            .unwrap()
            .unwrap();
        assert_eq!(plan.new_tag, "v2");
        assert_eq!(plan.new_sqlite_classes, ["Created"]);
        assert_eq!(plan.renamed_classes[0].to, "Renamed");

        let exports = metadata(serde_json::json!({
            "main_module": "index.js",
            "compatibility_date": "2026-09-08",
            "exports": {
                "default": {"type": "worker", "cache": {"enabled": true}},
                "NewName": {"type": "durable-object", "storage": "sqlite"},
                "OldName": {"type": "durable-object", "state": "renamed", "renamed_to": "NewName"},
                "Gone": {"type": "durable-object", "state": "deleted"}
            }
        }));
        let plan = migration_plan(&exports, Some("prior".to_owned()), Some(b"first code"))
            .unwrap()
            .unwrap();
        assert_eq!(plan.old_tag.as_deref(), Some("prior"));
        assert!(plan.new_sqlite_classes.is_empty());
        assert_eq!(plan.renamed_classes[0].from, "OldName");
        assert_eq!(plan.deleted_classes, ["Gone"]);
        assert!(declares_live_class(&exports, "NewName"));

        let mut replay = migration_plan(&exports, Some(plan.new_tag.clone()), Some(b"first code"))
            .unwrap()
            .unwrap();
        assert_eq!(replay.old_tag.as_ref(), Some(&plan.new_tag));
        normalize_declarative_replay_base(
            &mut replay,
            Some(&DurableObjectMigrationHead {
                tag: plan.new_tag.clone(),
                old_tag: plan.old_tag.clone(),
                plan_sha256: plan.fingerprint().unwrap(),
                version_id: open_compute_core::VersionId::generate(),
            }),
        );
        assert_eq!(replay.fingerprint().unwrap(), plan.fingerprint().unwrap());
        assert_ne!(
            migration_plan(&exports, Some(plan.new_tag.clone()), Some(b"changed code"))
                .unwrap()
                .unwrap()
                .new_tag,
            plan.new_tag
        );
    }

    #[test]
    fn non_sqlite_and_transfer_lifecycle_fail_closed() {
        for value in [
            serde_json::json!({
                "main_module": "index.js",
                "compatibility_date": "2026-09-08",
                "migrations": {"new_tag": "v1", "steps": [{"new_classes": ["Legacy"]}]}
            }),
            serde_json::json!({
                "main_module": "index.js",
                "compatibility_date": "2026-09-08",
                "exports": {"Moved": {"type": "durable-object", "state": "transferred", "transferred_to": "other"}}
            }),
        ] {
            assert!(migration_plan(&metadata(value), None, Some(b"code")).is_err());
        }
    }
}
