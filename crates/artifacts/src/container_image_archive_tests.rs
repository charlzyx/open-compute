use super::*;

#[test]
fn docker_materialization_retains_layer_compression_and_configuration_identity() {
    let first = tar(&[("app", tar::EntryType::Regular, None, b"main")]);
    let second = tar(&[("data", tar::EntryType::Regular, None, b"value")]);
    let graph = Graph::new(&[
        (first.clone(), false),
        (second.clone(), true),
        (second.clone(), true),
    ]);
    let image = graph.verify(limits()).unwrap();
    let oci = archive(&graph, &image).unwrap();
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(&oci).unwrap();
    let artifact = crate::ArtifactRef::new(1, &hex::encode(hash(&oci)), oci.len() as u64).unwrap();
    let converted = write_container_docker_archive(
        &file,
        &artifact,
        *image.manifest_sha256(),
        "arm64",
        limits(),
        Vec::new(),
    )
    .unwrap();
    let members = members(&converted);
    assert_eq!(members.len(), 4);
    let manifest: Value = serde_json::from_slice(&members["manifest.json"]).unwrap();
    assert_eq!(manifest[0]["RepoTags"], json!([]));
    let configuration = format!("{}.json", hex::encode(image.config().sha256));
    assert_eq!(manifest[0]["Config"], configuration);
    assert_eq!(members[&configuration], graph.blobs[&image.config().sha256]);
    for (index, expected) in [first, second.clone(), second].iter().enumerate() {
        let path = format!("{}/layer.tar", hex::encode(image.diff_ids()[index]));
        assert_eq!(manifest[0]["Layers"][index], path);
        assert_eq!(members[&path], graph.blobs[&image.layers()[index].sha256]);
        let mut unpacked = Vec::new();
        if index == 0 {
            unpacked.extend_from_slice(&members[&path]);
        } else {
            MultiGzDecoder::new(Cursor::new(&members[&path]))
                .read_to_end(&mut unpacked)
                .unwrap();
        }
        assert_eq!(unpacked, *expected);
        assert_eq!(hash(&unpacked), image.diff_ids()[index]);
    }
    struct FailingWriter(usize);
    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.0 == 0 {
                return Err(io::Error::other("private-engine-spool"));
            }
            let count = self.0.min(bytes.len());
            self.0 -= count;
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    for budget in [0, 1024, 2048, 2560, converted.len() - 1] {
        let error = write_container_docker_archive(
            &file,
            &artifact,
            *image.manifest_sha256(),
            "arm64",
            limits(),
            FailingWriter(budget),
        )
        .err()
        .unwrap();
        assert!(!error.to_string().contains("private-engine-spool"));
    }
    assert!(
        ContainerImageLimits {
            json_bytes: u64::MAX,
            ..limits()
        }
        .archive_bytes()
        .is_err()
    );
    assert!(
        ContainerImageLimits {
            layers: usize::MAX,
            ..limits()
        }
        .archive_bytes()
        .is_err()
    );
}

fn restore(
    bytes: &[u8],
    expected: [u8; 32],
    architecture: &str,
) -> Result<VerifiedContainerImage, PlatformError> {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(bytes).unwrap();
    let artifact =
        crate::ArtifactRef::new(1, &hex::encode(hash(bytes)), bytes.len() as u64).unwrap();
    verify_container_image_archive(&file, &artifact, expected, architecture, limits())
}

fn members(bytes: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let mut archive = tar::Archive::new(Cursor::new(bytes));
    archive
        .entries()
        .unwrap()
        .map(|entry| {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_str().unwrap().to_owned();
            let mut content = Vec::new();
            entry.read_to_end(&mut content).unwrap();
            (path, content)
        })
        .collect()
}

fn pack(members: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let entries = members
        .iter()
        .map(|(path, bytes)| {
            (
                path.as_str(),
                tar::EntryType::Regular,
                None,
                bytes.as_slice(),
            )
        })
        .collect::<Vec<_>>();
    tar(&entries)
}

#[test]
fn offline_restore_verifies_complete_graph_and_preserves_file_cursor() {
    use std::io::{Seek, SeekFrom};
    let graph = Graph::ordinary();
    let image = graph.verify(limits()).unwrap();
    let bytes = archive(&graph, &image).unwrap();
    assert_eq!(
        restore(&bytes, *image.manifest_sha256(), "arm64").unwrap(),
        image
    );
    assert_eq!(image.diff_ids().len(), image.layers().len());
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(&bytes).unwrap();
    file.seek(SeekFrom::Start(17)).unwrap();
    let artifact =
        crate::ArtifactRef::new(1, &hex::encode(hash(&bytes)), bytes.len() as u64).unwrap();
    verify_container_image_archive(
        &file,
        &artifact,
        *image.manifest_sha256(),
        "arm64",
        limits(),
    )
    .unwrap();
    assert_eq!(file.stream_position().unwrap(), 17);
    for artifact in [
        crate::ArtifactRef::new(1, &"a".repeat(64), bytes.len() as u64).unwrap(),
        crate::ArtifactRef::new(1, &hex::encode(hash(&bytes)), 0).unwrap(),
        crate::ArtifactRef::new(1, &hex::encode(hash(&bytes)), bytes.len() as u64 + 1).unwrap(),
        crate::ArtifactRef::new(1, &hex::encode(hash(&bytes)), u64::MAX).unwrap(),
    ] {
        assert!(
            verify_container_image_archive(
                &file,
                &artifact,
                *image.manifest_sha256(),
                "arm64",
                limits()
            )
            .is_err()
        );
    }
    assert!(restore(&bytes, [0; 32], "arm64").is_err());
    assert!(restore(&bytes, *image.manifest_sha256(), "amd64").is_err());
}

#[test]
fn offline_restore_rejects_invalid_layout_unreferenced_members_and_inner_tampering() {
    let graph = Graph::ordinary();
    let image = graph.verify(limits()).unwrap();
    let original = members(&archive(&graph, &image).unwrap());
    for mutation in 0..16 {
        let mut changed = original.clone();
        match mutation {
            0 => {
                changed.remove("oci-layout");
            }
            1 => {
                changed.insert("oci-layout".into(), Vec::new());
            }
            2 => {
                changed.insert(
                    "oci-layout".into(),
                    br#"{"imageLayoutVersion":"2.0.0"}"#.to_vec(),
                );
            }
            3 => {
                changed.insert(
                    "oci-layout".into(),
                    br#"{"imageLayoutVersion":"1.0.0","unknown":true}"#.to_vec(),
                );
            }
            4 => {
                changed.insert("oci-layout".into(), vec![0; 8193]);
            }
            5 => {
                changed.insert(
                    format!("blobs/sha256/{}", "0".repeat(64)),
                    b"unreferenced".to_vec(),
                );
            }
            6 => {
                changed.insert("unexpected".into(), b"extra".to_vec());
            }
            7 => {
                changed
                    .get_mut(&format!(
                        "blobs/sha256/{}",
                        hex::encode(image.config().sha256)
                    ))
                    .unwrap()[0] ^= 1;
            }
            8 => {
                changed.remove(&format!(
                    "blobs/sha256/{}",
                    hex::encode(image.layers()[0].sha256)
                ));
            }
            _ => {
                let mut index: Value = serde_json::from_slice(&changed["index.json"]).unwrap();
                match mutation {
                    9 => index["schemaVersion"] = json!(1),
                    10 => index["manifests"] = json!([]),
                    11 => index["manifests"][0]["digest"] = json!("invalid"),
                    12 => index["manifests"][0]["size"] = json!(1),
                    13 => index["manifests"][0]["mediaType"] = json!("other"),
                    14 => index["manifests"][0]["platform"]["os"] = json!("windows"),
                    _ => index["unknown"] = json!(true),
                }
                changed.insert("index.json".into(), serde_json::to_vec(&index).unwrap());
            }
        }
        assert!(
            restore(&pack(&changed), *image.manifest_sha256(), "arm64").is_err(),
            "mutation {mutation}"
        );
    }
    let invalid = tar(&[("oci-layout", tar::EntryType::Directory, None, b"")]);
    assert!(restore(&invalid, *image.manifest_sha256(), "arm64").is_err());
}

#[test]
fn container_tar_inspection_requires_bounded_complete_content_without_host_extraction() {
    let bytes = tar(&[("app", tar::EntryType::Regular, None, b"safe")]);
    let inspected = verify_container_archive(Cursor::new(&bytes), bytes.len() as u64, 1).unwrap();
    assert_eq!(inspected.sha256, hash(&bytes));
    assert_eq!(inspected.bytes, bytes.len() as u64);
    for (max_bytes, max_entries) in [(0, 1), (bytes.len() as u64 - 1, 1), (65536, 0)] {
        assert!(verify_container_archive(Cursor::new(&bytes), max_bytes, max_entries).is_err());
    }
    let invalid = tar(&[("device", tar::EntryType::Block, None, b"")]);
    assert!(verify_container_archive(Cursor::new(invalid), 65536, 1).is_err());
}

#[test]
fn container_tar_inspection_batches_reads_without_reading_past_the_byte_bound() {
    struct CountedReader<'a> {
        inner: Cursor<Vec<u8>>,
        calls: &'a std::cell::Cell<usize>,
        bytes: &'a std::cell::Cell<usize>,
    }
    impl Read for CountedReader<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.calls.set(self.calls.get() + 1);
            let count = self.inner.read(output)?;
            self.bytes.set(self.bytes.get() + count);
            Ok(count)
        }
    }

    let names = (0..100).map(|n| format!("entry-{n}")).collect::<Vec<_>>();
    let entries = names
        .iter()
        .map(|name| {
            (
                name.as_str(),
                tar::EntryType::Regular,
                None,
                b"x".as_slice(),
            )
        })
        .collect::<Vec<_>>();
    let archive = tar(&entries);
    let calls = std::cell::Cell::new(0);
    let bytes = std::cell::Cell::new(0);
    for maximum in [archive.len() as u64, 1] {
        calls.set(0);
        bytes.set(0);
        let reader = CountedReader {
            inner: Cursor::new(archive.clone()),
            calls: &calls,
            bytes: &bytes,
        };
        let result = verify_container_archive(reader, maximum, entries.len());
        if maximum == 1 {
            assert!(result.is_err());
            assert_eq!(bytes.get(), 2);
        } else {
            let result = result.unwrap();
            assert_eq!(result.sha256, hash(&archive));
            assert_eq!(result.bytes, archive.len() as u64);
            assert!(calls.get() < entries.len());
        }
    }
}

fn archive(graph: &Graph, image: &VerifiedContainerImage) -> Result<Vec<u8>, PlatformError> {
    write_container_image_archive(
        image,
        &serde_json::to_vec(&graph.manifest).unwrap(),
        |blob| {
            graph
                .blobs
                .get(&blob.sha256)
                .cloned()
                .map(Cursor::new)
                .ok_or_else(integrity_error)
        },
        Vec::new(),
    )
}

#[test]
fn offline_archive_contains_only_the_verified_graph_and_has_reproducible_bytes() {
    let layer = tar(&[("app", tar::EntryType::Regular, None, b"executable")]);
    let graph = Graph::new(&[(layer.clone(), true), (layer, true)]);
    let image = graph.verify(limits()).unwrap();
    let bytes = archive(&graph, &image).unwrap();
    assert_eq!(bytes, archive(&graph, &image).unwrap());
    let mut members = BTreeMap::new();
    let mut archive = tar::Archive::new(Cursor::new(bytes));
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        assert_eq!(entry.header().uid().unwrap(), 0);
        assert_eq!(entry.header().gid().unwrap(), 0);
        assert_eq!(entry.header().mtime().unwrap(), 0);
        assert_eq!(entry.header().mode().unwrap(), 0o644);
        assert!(entry.header().entry_type().is_file());
        let path = entry.path().unwrap().into_owned();
        let mut body = Vec::new();
        entry.read_to_end(&mut body).unwrap();
        assert!(members.insert(path, body).is_none());
    }
    assert_eq!(members.len(), graph.blobs.len() + 3);
    assert_eq!(
        members[std::path::Path::new("oci-layout")],
        br#"{"imageLayoutVersion":"1.0.0"}"#
    );
    let index: Value =
        serde_json::from_slice(&members[std::path::Path::new("index.json")]).unwrap();
    assert_eq!(index["schemaVersion"], 2);
    assert_eq!(index["manifests"][0]["digest"], graph.manifest_digest());
    assert_eq!(index["manifests"][0]["platform"]["architecture"], "arm64");
    let manifest_path = format!("blobs/sha256/{}", hex::encode(image.manifest_sha256()));
    assert_eq!(
        members[std::path::Path::new(&manifest_path)],
        serde_json::to_vec(&graph.manifest).unwrap()
    );
    for (digest, expected) in &graph.blobs {
        let path = format!("blobs/sha256/{}", hex::encode(digest));
        assert_eq!(members[std::path::Path::new(&path)], *expected);
    }
}

impl Graph {
    fn manifest_digest(&self) -> String {
        token(&serde_json::to_vec(&self.manifest).unwrap())
    }
}

#[test]
fn offline_archive_rejects_tampering_between_verification_and_publication() {
    let graph = Graph::ordinary();
    let image = graph.verify(limits()).unwrap();
    let original_manifest = serde_json::to_vec(&graph.manifest).unwrap();
    for manifest in [b"different".to_vec(), {
        let mut bytes = original_manifest.clone();
        bytes[0] ^= 1;
        bytes
    }] {
        assert!(
            write_container_image_archive(
                &image,
                &manifest,
                |_| Ok(Cursor::new(Vec::new())),
                Vec::new()
            )
            .is_err()
        );
    }
    for blob in std::iter::once(image.config()).chain(image.layers()) {
        for mutation in 0..3 {
            let mut changed = Graph::ordinary();
            let bytes = changed.blobs.get_mut(&blob.sha256).unwrap();
            match mutation {
                0 => bytes[0] ^= 1,
                1 => bytes.push(0),
                _ => {
                    bytes.pop();
                }
            }
            assert!(archive(&changed, &image).is_err());
        }
        let mut missing = Graph::ordinary();
        missing.blobs.remove(&blob.sha256);
        assert!(archive(&missing, &image).is_err());
    }
}

#[test]
fn offline_archive_sanitizes_read_and_write_failures() {
    struct Failure(usize);
    impl Write for Failure {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.0 == 0 {
                return Err(io::Error::other("private-output-path"));
            }
            let count = bytes.len().min(self.0);
            self.0 -= count;
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("private-output-path"))
        }
    }
    let graph = Graph::ordinary();
    let image = graph.verify(limits()).unwrap();
    let bytes = serde_json::to_vec(&graph.manifest).unwrap();
    let output = archive(&graph, &image).unwrap();
    for fail_after in [0, 1024, 2560, 4096, output.len() - 1] {
        let error = write_container_image_archive(
            &image,
            &bytes,
            |blob| Ok(Cursor::new(graph.blobs[&blob.sha256].clone())),
            Failure(fail_after),
        )
        .err()
        .unwrap();
        assert!(!error.to_string().contains("private-output-path"));
    }
    let error = write_container_image_archive::<_, Cursor<Vec<u8>>>(
        &image,
        &bytes,
        |_| {
            Err(PlatformError::new(
                open_compute_core::ErrorCode::PathInvalid,
                "private-blob-path",
            ))
        },
        Vec::new(),
    )
    .unwrap_err();
    assert!(!error.to_string().contains("private-blob-path"));
}
