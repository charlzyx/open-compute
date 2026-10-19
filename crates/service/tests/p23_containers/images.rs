use super::http::{Client, json, request};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::path::PathBuf;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Image {
    pub manifest: PathBuf,
    pub blobs: Vec<Blob>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Blob {
    pub path: PathBuf,
    pub sha256: String,
}

impl Image {
    pub(super) async fn push(&self, client: &Client, admin: &str, account: &str) -> String {
        let credentials = json(
            client,
            admin,
            admin,
            &format!("/client/v4/accounts/{account}/containers/registries/{admin}/credentials"),
            "POST",
            Some("Bearer workflow-deployer"),
            serde_json::json!({"expiration_minutes":15,"permissions":["push","pull"]}),
        )
        .await;
        assert_eq!(credentials["result"]["username"], account);
        let authorization = format!(
            "Bearer {}",
            credentials["result"]["password"].as_str().unwrap()
        );
        assert!(!self.blobs.is_empty() && self.blobs.len() <= 129);
        let graph_bytes = self
            .blobs
            .iter()
            .try_fold(
                std::fs::metadata(&self.manifest).unwrap().len(),
                |total, blob| total.checked_add(std::fs::metadata(&blob.path).unwrap().len()),
            )
            .unwrap();
        assert!(
            graph_bytes <= 1 << 30,
            "prepared OCI graph exceeds the explicit 1 GiB budget"
        );
        let repository = format!("{account}/containers-fixture");
        for blob in &self.blobs {
            assert!(blob.path.is_absolute());
            assert!(std::fs::metadata(&blob.path).unwrap().len() <= 1 << 30);
            let bytes = std::fs::read(&blob.path).unwrap();
            assert_eq!(hex::encode(Sha256::digest(&bytes)), blob.sha256);
            let (status, headers, _) = request(
                client,
                admin,
                admin,
                &format!("/v2/{repository}/blobs/uploads/"),
                "POST",
                Some(&authorization),
                "application/octet-stream",
                Vec::new(),
            )
            .await;
            assert_eq!(status, 202);
            let location = headers["location"].to_str().unwrap();
            assert!(location.starts_with(&format!("/v2/{repository}/blobs/uploads/")));
            let path = format!("{location}?digest=sha256:{}", blob.sha256);
            let (status, _, _) = request(
                client,
                admin,
                admin,
                &path,
                "PUT",
                Some(&authorization),
                "application/octet-stream",
                bytes,
            )
            .await;
            assert_eq!(status, 201);
        }
        assert!(self.manifest.is_absolute());
        assert!(std::fs::metadata(&self.manifest).unwrap().len() <= 1 << 20);
        let manifest = std::fs::read(&self.manifest).unwrap();
        let digest = hex::encode(Sha256::digest(&manifest));
        let path = format!("/v2/{repository}/manifests/sha256:{digest}");
        let (status, _, _) = request(
            client,
            admin,
            admin,
            &path,
            "PUT",
            Some(&authorization),
            "application/vnd.oci.image.manifest.v1+json",
            manifest.clone(),
        )
        .await;
        assert_eq!(status, 201, "complete immutable OCI graph must be admitted");
        let (status, _, admitted) = request(
            client,
            admin,
            admin,
            &path,
            "GET",
            Some(&authorization),
            "application/json",
            Vec::new(),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(admitted, manifest);
        let (unauthorized, _, _) = request(
            client,
            admin,
            admin,
            &path,
            "GET",
            None,
            "application/json",
            Vec::new(),
        )
        .await;
        assert_eq!(unauthorized, 401);
        let mut corrupted = manifest;
        corrupted.push(b' ');
        let (rejected, _, _) = request(
            client,
            admin,
            admin,
            &path,
            "PUT",
            Some(&authorization),
            "application/vnd.oci.image.manifest.v1+json",
            corrupted,
        )
        .await;
        assert_eq!(
            rejected, 400,
            "digest mismatch must not mutate admitted image authority"
        );
        format!("{admin}/{repository}@sha256:{digest}")
    }
}
