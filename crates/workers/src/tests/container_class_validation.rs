use super::*;
use open_compute_core::{CanonicalBindingConfig, CanonicalPermissions, PlatformError, ResourceId};
use open_compute_storage::containers::{ContainerAuthority, VersionContainerDeclaration};
use open_compute_storage::durable_objects::DO_NAMESPACE_SCHEMA_VERSION;
use std::pin::Pin;
use std::sync::Mutex;

struct Fixture {
    _directory: tempfile::TempDir,
    _mock: MockS3,
    storage: Arc<PlatformStorage>,
    artifacts: ArtifactStore,
    worker: WorkerId,
    namespace: ResourceId,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let storage = Arc::new(
            PlatformStorage::bootstrap(
                &storage_config(&directory.path().join("data")),
                &SystemClock,
            )
            .unwrap(),
        );
        let account = storage.identity().instance_id;
        let worker = WorkerRepository::new(storage.db())
            .create_worker(
                account,
                "container-validation",
                RequestId::generate(),
                1,
                1_000_000,
            )
            .unwrap()
            .0
            .id;
        let namespace = match ResourceController::new(
            &storage,
            ResourcePins::new(),
            DurableObjectResourceDriver::new(&storage, worker, "ContainerClass"),
        )
        .create(&CreateResourceRequest {
            instance_id: account,
            kind: BindingKind::DoNamespace,
            name: "container-class".into(),
            idempotency_key: "container-class".into(),
            driver_schema_version: DO_NAMESPACE_SCHEMA_VERSION,
            request_id: RequestId::generate(),
            now_ms: 2,
        })
        .unwrap()
        {
            CreateResourceOutcome::Applied(result) => result.resource_id,
            other => panic!("unexpected namespace result: {other:?}"),
        };
        let mock = MockS3::spawn("open-compute").await;
        let artifacts = artifact_store(&mock);
        Self {
            _directory: directory,
            _mock: mock,
            storage,
            artifacts,
            worker,
            namespace,
        }
    }

    fn request(&self, key: &str, source: &[u8]) -> CreateVersionRequest {
        let mut request = version_request(
            self.storage.identity().instance_id,
            self.worker,
            key,
            "secret",
        );
        request.deployment_source = None;
        request.content = VersionContent::Worker {
            bundle: CanonicalBundle::build(
                "index.js",
                vec![module("index.js", source)],
                BundleLimits::default(),
            )
            .unwrap()
            .into_bytes()
            .into(),
            assets: None,
        };
        request.runtime_features.containers =
            vec![VersionContainerDeclaration::new(self.namespace, "container-app".into()).unwrap()];
        assert!(request.bindings.is_empty());
        request
    }
}

// This fixture proves pipeline delegation and error handling, not native JavaScript class typing.
struct ClassValidator {
    error: Option<ErrorCode>,
    classes: Mutex<Vec<String>>,
}

impl RuntimeValidator for ClassValidator {
    fn validate(
        &self,
        _candidate: ValidationCandidate,
    ) -> Pin<Box<dyn Future<Output = Result<(), PlatformError>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }

    fn validate_deployment(
        &self,
        _candidate: ValidationCandidate,
    ) -> Pin<Box<dyn Future<Output = Result<StartupId, PlatformError>> + Send + '_>> {
        Box::pin(async { Ok(test_startup_id()) })
    }

    fn current_generation(&self) -> Option<StartupId> {
        Some(test_startup_id())
    }

    fn validate_durable_object_class(
        &self,
        _candidate: ValidationCandidate,
        class_name: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), PlatformError>> + Send + '_>> {
        self.classes.lock().unwrap().push(class_name);
        let error = self.error;
        Box::pin(async move {
            error.map_or(Ok(()), |code| {
                Err(PlatformError::new(code, "class probe rejected export"))
            })
        })
    }
}

#[tokio::test]
async fn container_exports_without_env_bindings_require_a_successful_class_probe() {
    for (source, code) in [
        (b"export default {};".as_slice(), ErrorCode::DoClassNotFound),
        (
            b"export const ContainerClass = 42; export default {};".as_slice(),
            ErrorCode::DoClassNotFound,
        ),
        (
            b"export class ContainerClass {} export default {};".as_slice(),
            ErrorCode::RuntimeUnavailable,
        ),
    ] {
        let fixture = Fixture::new().await;
        let request = fixture.request("rejected-container", source);
        let validator = Arc::new(ClassValidator {
            error: Some(code),
            classes: Mutex::new(Vec::new()),
        });
        let result = VersionController::new(
            &fixture.storage,
            fixture.artifacts.clone(),
            validator.clone(),
            BundleLimits::default(),
        )
        .create_version(request)
        .await;
        assert_eq!(result.unwrap_err().code(), code);
        assert_eq!(*validator.classes.lock().unwrap(), ["ContainerClass"]);
        let repository = WorkerRepository::new(fixture.storage.db());
        let account = fixture.storage.identity().instance_id;
        let versions = repository.list_versions(account, fixture.worker).unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].state, VersionState::Rejected);
        assert_eq!(versions[0].rejection_code.as_deref(), Some(code.as_str()));
        assert!(
            repository
                .get_worker(account, fixture.worker)
                .unwrap()
                .active_version_id
                .is_none()
        );
        assert!(
            ContainerAuthority::new(fixture.storage.db())
                .applications(account)
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn container_classes_are_probed_once_alongside_matching_namespace_bindings() {
    let fixture = Fixture::new().await;
    let mut request = fixture.request(
        "shared-container-class",
        b"export class ContainerClass {} export default {};",
    );
    request.bindings.insert(
        "CLASS".into(),
        VersionBindingInput {
            kind: BindingKind::DoNamespace,
            id: fixture.namespace,
            permissions: CanonicalPermissions::default(),
            config: CanonicalBindingConfig::default(),
        },
    );
    let validator = Arc::new(ClassValidator {
        error: None,
        classes: Mutex::new(Vec::new()),
    });
    let outcome = VersionController::new(
        &fixture.storage,
        fixture.artifacts.clone(),
        validator.clone(),
        BundleLimits::default(),
    )
    .create_version(request)
    .await
    .unwrap();
    let CreateVersionOutcome::Applied(result) = outcome else {
        panic!("unexpected replay")
    };
    assert_eq!(result.version.state, VersionState::Ready);
    assert_eq!(*validator.classes.lock().unwrap(), ["ContainerClass"]);
    assert_eq!(
        ContainerAuthority::new(fixture.storage.db())
            .version_declarations(fixture.storage.identity().instance_id, result.version.id,)
            .unwrap()
            .len(),
        1,
    );
}

#[tokio::test]
async fn cancelled_container_validation_resumes_from_persisted_declarations() {
    let fixture = Fixture::new().await;
    let request = fixture.request("resume-container", b"export default {};");
    let version_id = VersionId::generate();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let started = Arc::new(Mutex::new(Some(started_tx)));
    let blocking: Arc<dyn RuntimeValidator> = Arc::new(move |_: ValidationCandidate| {
        let started = started.lock().unwrap().take();
        async move {
            if let Some(started) = started {
                let _ = started.send(());
            }
            std::future::pending::<Result<(), PlatformError>>().await
        }
    });
    let storage = fixture.storage.clone();
    let artifacts = fixture.artifacts.clone();
    let first_request = request.clone();
    let attempt = tokio::spawn(async move {
        VersionController::new(&storage, artifacts, blocking, BundleLimits::default())
            .finalize_upload(first_request, version_id)
            .await
    });
    started_rx.await.unwrap();
    attempt.abort();
    assert!(attempt.await.unwrap_err().is_cancelled());
    let account = fixture.storage.identity().instance_id;
    let repository = WorkerRepository::new(fixture.storage.db());
    assert_eq!(
        repository
            .get_version(account, fixture.worker, version_id)
            .unwrap()
            .state,
        VersionState::Validating
    );
    assert_eq!(
        ContainerAuthority::new(fixture.storage.db())
            .version_declarations(account, version_id)
            .unwrap(),
        request.runtime_features.containers,
    );
    let validator = Arc::new(ClassValidator {
        error: Some(ErrorCode::DoClassNotFound),
        classes: Mutex::new(Vec::new()),
    });
    let result = VersionController::new(
        &fixture.storage,
        fixture.artifacts.clone(),
        validator.clone(),
        BundleLimits::default(),
    )
    .finalize_upload(request, version_id)
    .await;
    assert_eq!(result.unwrap_err().code(), ErrorCode::DoClassNotFound);
    assert_eq!(*validator.classes.lock().unwrap(), ["ContainerClass"]);
    let versions = repository.list_versions(account, fixture.worker).unwrap();
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0].id, version_id);
    assert_eq!(versions[0].state, VersionState::Rejected);
    assert!(
        repository
            .get_worker(account, fixture.worker)
            .unwrap()
            .active_version_id
            .is_none()
    );
}
