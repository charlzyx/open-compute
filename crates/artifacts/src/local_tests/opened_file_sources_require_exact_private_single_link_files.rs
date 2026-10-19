use super::*;

#[tokio::test]
async fn opened_file_sources_require_exact_private_single_link_files() {
    let fixture = Fixture::new();
    let key = ObjectKey::new("system/source/value").unwrap();
    let path = fixture.config.path.parent().unwrap().join("source");
    fs::write(&path, b"source").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let mismatched = OpenOptions::new().read(true).open(&path).unwrap();
    assert_eq!(
        fixture
            .backend
            .put(
                &key,
                ObjectSource::File {
                    file: mismatched,
                    length: 5,
                },
                options(PutMode::Replace),
            )
            .await
            .unwrap_err(),
        BackendError::Corrupt
    );
    let link = path.with_extension("link");
    fs::hard_link(&path, &link).unwrap();
    let linked = OpenOptions::new().read(true).open(&path).unwrap();
    assert_eq!(
        fixture
            .backend
            .put(
                &key,
                ObjectSource::File {
                    file: linked,
                    length: 6,
                },
                options(PutMode::Replace),
            )
            .await
            .unwrap_err(),
        BackendError::Corrupt
    );
    fs::remove_file(link).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let public = OpenOptions::new().read(true).open(&path).unwrap();
    assert_eq!(
        fixture
            .backend
            .put(
                &key,
                ObjectSource::File {
                    file: public,
                    length: 6,
                },
                options(PutMode::Replace),
            )
            .await
            .unwrap_err(),
        BackendError::Corrupt
    );
}

#[tokio::test]
async fn anonymous_verified_file_publication_uses_the_local_backend() {
    use std::io::Write as _;

    let fixture = Fixture::new();
    let store = crate::ArtifactStore::new(fixture.backend.clone());
    let payload = b"anonymous-local-artifact";
    let mut file = tempfile::tempfile().unwrap();
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .unwrap();
    file.write_all(payload).unwrap();
    let mut called = false;
    let artifact = store
        .put_verified_open_file(
            file,
            &hex::encode(sha2::Sha256::digest(payload)),
            payload.len() as u64,
            || {
                called = true;
                Ok(())
            },
        )
        .await
        .unwrap();
    assert!(called);
    assert_eq!(store.open(&artifact).await.unwrap().as_ref(), payload);
}
