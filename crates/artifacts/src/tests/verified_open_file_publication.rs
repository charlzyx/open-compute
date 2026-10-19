use super::*;
use open_compute_core::PlatformError;
use std::io::{Seek as _, SeekFrom};
use std::os::unix::fs::{FileExt as _, MetadataExt as _};

fn anonymous(payload: &[u8]) -> fs::File {
    let mut file = tempfile::tempfile().unwrap();
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .unwrap();
    file.write_all(payload).unwrap();
    assert_eq!(file.metadata().unwrap().nlink(), 0);
    file
}

#[tokio::test]
async fn anonymous_file_publication_marks_after_missing_preflight_and_uploads_from_zero() {
    let mock = MockS3::spawn("open-compute").await;
    let store = ArtifactStore::new(client_for(&mock).await);
    let payload = vec![b'x'; 48 * 1024];
    let digest = hex::encode(Sha256::digest(&payload));
    let mut file = anonymous(&payload);
    file.seek(SeekFrom::End(0)).unwrap();
    let mut calls = 0;
    let artifact = store
        .put_verified_open_file(file, &digest, payload.len() as u64, || {
            let requests = mock.recorded();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].method, "HEAD");
            assert_eq!(mock.object_count(), 0);
            calls += 1;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(store.open(&artifact).await.unwrap().as_ref(), payload);
    assert_eq!(
        mock.recorded()
            .iter()
            .filter(|row| row.method == "PUT")
            .count(),
        1
    );

    mock.clear_recorded();
    let existing = store
        .put_verified_open_file(anonymous(&payload), &digest, payload.len() as u64, || {
            panic!("verified existing objects must not mark a PUT attempt")
        })
        .await
        .unwrap();
    assert_eq!(existing, artifact);
    let methods: Vec<_> = mock.recorded().into_iter().map(|row| row.method).collect();
    assert_eq!(methods, ["HEAD", "GET"]);
}

#[tokio::test]
async fn refused_publication_callback_never_sends_put() {
    let mock = MockS3::spawn("open-compute").await;
    let store = ArtifactStore::new(client_for(&mock).await);
    let payload = b"refused-publication";
    let failure = store
        .put_verified_open_file(
            anonymous(payload),
            &hex::encode(Sha256::digest(payload)),
            payload.len() as u64,
            || {
                Err(PlatformError::new(
                    ErrorCode::AdmissionBusy,
                    "publication refused",
                ))
            },
        )
        .await
        .unwrap_err();
    assert_eq!(failure.code(), ErrorCode::AdmissionBusy);
    assert_eq!(mock.object_count(), 0);
    assert!(mock.recorded().iter().all(|row| row.method == "HEAD"));
}

#[tokio::test]
async fn open_file_tampering_before_or_during_callback_never_sends_put() {
    let mock = MockS3::spawn("open-compute").await;
    let store = ArtifactStore::new(client_for(&mock).await);
    let payload = b"original";
    let digest = hex::encode(Sha256::digest(payload));
    for after_preflight in [false, true] {
        mock.clear_recorded();
        let file = anonymous(payload);
        let writer = file.try_clone().unwrap();
        if !after_preflight {
            writer.write_all_at(b"tampered", 0).unwrap();
        }
        let mut called = false;
        let failure = store
            .put_verified_open_file(file, &digest, payload.len() as u64, || {
                called = true;
                writer.write_all_at(b"tampered", 0).unwrap();
                Ok(())
            })
            .await
            .unwrap_err();
        assert_eq!(failure.code(), ErrorCode::ArtifactIntegrityError);
        assert_eq!(called, after_preflight);
        assert_eq!(mock.object_count(), 0);
        assert!(mock.recorded().iter().all(|row| row.method == "HEAD"));
    }
}

#[tokio::test]
async fn preflight_failures_and_corrupt_existing_objects_do_not_mark_publication() {
    let mock = MockS3::spawn("open-compute").await;
    let store = ArtifactStore::new(client_for(&mock).await);
    let payload = b"existing";
    let digest = hex::encode(Sha256::digest(payload));
    let artifact = store
        .put_verified_open_file(anonymous(payload), &digest, payload.len() as u64, || Ok(()))
        .await
        .unwrap();
    mock.put_raw(&artifact.physical_key("system/"), b"tampered".to_vec());
    for fault in [Fault::None, Fault::ServerError] {
        mock.set_fault(fault);
        mock.clear_recorded();
        assert!(
            store
                .put_verified_open_file(anonymous(payload), &digest, payload.len() as u64, || {
                    panic!("failed preflight must not mark a PUT attempt")
                })
                .await
                .is_err()
        );
        assert!(mock.recorded().iter().all(|row| row.method != "PUT"));
    }
}

#[tokio::test]
async fn lost_put_response_can_commit_after_client_failure_and_positive_absence() {
    let mock = MockS3::spawn("open-compute").await;
    let mut config = s3_config(&mock.endpoint);
    config.max_retries = 0;
    let credentials = resolve_s3_credentials_with(&config, &env()).unwrap();
    let store =
        ArtifactStore::new(ObjectBackend::connect_s3(&config, &credentials, 64 * 1024).unwrap());
    let payload = b"late-artifact";
    let digest = hex::encode(Sha256::digest(payload));
    let artifact = ArtifactRef::new(1, &digest, payload.len() as u64).unwrap();
    mock.set_fault(Fault::LatePutResponseLoss);
    let uploading = store.clone();
    let publication = tokio::spawn(async move {
        uploading
            .put_verified_open_file(anonymous(payload), &digest, payload.len() as u64, || Ok(()))
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), mock.wait_for_late_put())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(3), publication)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(store.is_absent(&artifact).await.unwrap());
    mock.release_late_put();
    tokio::time::timeout(Duration::from_secs(3), mock.wait_for_late_put_commit())
        .await
        .unwrap();
    assert!(!store.is_absent(&artifact).await.unwrap());
    assert_eq!(store.open(&artifact).await.unwrap().as_ref(), payload);
}
