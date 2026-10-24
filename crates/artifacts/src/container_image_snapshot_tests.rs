use super::*;
use std::fs::File;
use std::io::{Seek, SeekFrom};

fn spool(bytes: &[u8]) -> File {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(bytes).unwrap();
    file
}

pub(super) fn sealed(graph: &Graph) -> (File, crate::ArtifactRef, VerifiedContainerImage) {
    let image = graph.verify(limits()).unwrap();
    let manifest = serde_json::to_vec(&graph.manifest).unwrap();
    let bytes = write_container_image_archive(
        &image,
        &manifest,
        |blob| Ok(Cursor::new(&graph.blobs[&blob.sha256])),
        Vec::new(),
    )
    .unwrap();
    let artifact =
        crate::ArtifactRef::new(1, &hex::encode(hash(&bytes)), bytes.len() as u64).unwrap();
    (spool(&bytes), artifact, image)
}

#[test]
fn offset_reader_seeks_without_moving_the_original_cursor_or_accepting_overflow() {
    let mut file = spool(b"abcdef");
    file.seek(SeekFrom::Start(3)).unwrap();
    let mut reader = restore::FileReader::new(&file, 0).unwrap();
    assert!(reader.seek(SeekFrom::Current(-1)).is_err());
    assert_eq!(reader.stream_position().unwrap(), 0);
    assert_eq!(reader.seek(SeekFrom::Start(2)).unwrap(), 2);
    let mut bytes = [0; 2];
    reader.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"cd");
    assert_eq!(reader.seek(SeekFrom::Current(-3)).unwrap(), 1);
    reader.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"bc");
    assert_eq!(reader.seek(SeekFrom::End(-2)).unwrap(), 4);
    reader.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"ef");
    assert_eq!(reader.read(&mut bytes).unwrap(), 0);
    assert!(reader.seek(SeekFrom::End(-7)).is_err());
    assert_eq!(reader.stream_position().unwrap(), 6);
    reader.seek(SeekFrom::Start(u64::MAX)).unwrap();
    assert!(reader.seek(SeekFrom::Current(1)).is_err());
    assert_eq!(reader.stream_position().unwrap(), u64::MAX);
    assert_eq!(file.stream_position().unwrap(), 3);
}

#[test]
fn snapshot_preserves_rootfs_bytes_and_sealed_configuration_without_runtime_options() {
    let mut graph = Graph::ordinary();
    graph.edit_config(|config| {
        config["config"]["User"] = json!("1000:1000");
        config["config"]["WorkingDir"] = json!("/app");
        config["config"]["Labels"] = json!({"image":"sealed"});
        config["history"] = json!([{"created_by":"old-layer"}]);
    });
    let (mut source, artifact, expected) = sealed(&graph);
    source.seek(SeekFrom::Start(17)).unwrap();
    let (image, config) = read_container_image_configuration(
        &source,
        &artifact,
        *expected.manifest_sha256(),
        "arm64",
        limits(),
    )
    .unwrap();
    assert_eq!(image, expected);
    assert_eq!(source.stream_position().unwrap(), 17);
    assert_eq!(config, graph.blobs[&image.config().sha256]);
    let rootfs = tar(&[
        ("app/", tar::EntryType::Directory, None, b""),
        ("app/state", tar::EntryType::Regular, None, b"saved"),
        ("app/hard", tar::EntryType::Link, Some("app/state"), b""),
        ("app/link", tar::EntryType::Symlink, Some("/app/state"), b""),
    ]);
    let mut root = spool(&rootfs);
    root.seek(SeekFrom::Start(23)).unwrap();
    let compressed = spool(&[]);
    let retained_compressed = compressed.try_clone().unwrap();
    let (snapshot, archive) =
        write_container_snapshot_image(&image, &config, &root, compressed, limits(), Vec::new())
            .unwrap();
    assert_eq!(root.stream_position().unwrap(), 23);
    assert_eq!(snapshot.layers().len(), 1);
    assert_ne!(snapshot.layers()[0].sha256, hash(&rootfs));
    assert_eq!(
        retained_compressed.metadata().unwrap().len(),
        snapshot.layers()[0].bytes
    );
    assert_eq!(snapshot.diff_ids(), &[hash(&rootfs)]);
    assert_eq!(snapshot.unpacked_bytes(), rootfs.len() as u64);
    let artifact =
        crate::ArtifactRef::new(1, &hex::encode(hash(&archive)), archive.len() as u64).unwrap();
    let (verified, saved) = read_container_image_configuration(
        &spool(&archive),
        &artifact,
        *snapshot.manifest_sha256(),
        "arm64",
        limits(),
    )
    .unwrap();
    assert_eq!(verified, snapshot);
    let old: Value = serde_json::from_slice(&config).unwrap();
    let saved: Value = serde_json::from_slice(&saved).unwrap();
    assert_eq!(saved["config"], old["config"]);
    assert!(saved.get("history").is_none());
    let mut entries = tar::Archive::new(Cursor::new(&archive));
    let path = format!("blobs/sha256/{}", hex::encode(snapshot.layers()[0].sha256));
    let mut layer = entries
        .entries()
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| entry.path().unwrap().to_str() == Some(path.as_str()))
        .unwrap();
    let mut compressed_rootfs = Vec::new();
    layer.read_to_end(&mut compressed_rootfs).unwrap();
    assert_eq!(hash(&compressed_rootfs), snapshot.layers()[0].sha256);
    let mut saved_rootfs = Vec::new();
    MultiGzDecoder::new(Cursor::new(compressed_rootfs))
        .read_to_end(&mut saved_rootfs)
        .unwrap();
    assert_eq!(saved_rootfs, rootfs);
    assert_eq!(
        write_container_snapshot_image(&image, &config, &root, spool(&[]), limits(), Vec::new())
            .unwrap()
            .1,
        archive
    );
}

#[test]
fn snapshot_bounds_compressed_and_unpacked_bytes_independently() {
    let graph = Graph::ordinary();
    let image = graph.verify(limits()).unwrap();
    let config = &graph.blobs[&image.config().sha256];
    let payload = vec![b'x'; 8 << 20];
    let rootfs = tar(&[("payload", tar::EntryType::Regular, None, &payload)]);
    let bounds = ContainerImageLimits {
        compressed_bytes: 128 << 10,
        unpacked_bytes: rootfs.len() as u64,
        ..limits()
    };
    let (snapshot, archive) = write_container_snapshot_image(
        &image,
        config,
        &spool(&rootfs),
        spool(&[]),
        bounds,
        Vec::new(),
    )
    .unwrap();
    assert_eq!(snapshot.unpacked_bytes(), rootfs.len() as u64);
    assert_eq!(snapshot.diff_ids(), &[hash(&rootfs)]);
    assert!(snapshot.layers()[0].bytes < bounds.compressed_bytes);
    let artifact =
        crate::ArtifactRef::new(1, &hex::encode(hash(&archive)), archive.len() as u64).unwrap();
    let verified = verify_container_image_archive(
        &spool(&archive),
        &artifact,
        *snapshot.manifest_sha256(),
        "arm64",
        bounds,
    )
    .unwrap();
    assert_eq!(verified, snapshot);
    assert_eq!(
        write_container_snapshot_image(
            &image,
            config,
            &spool(&rootfs),
            spool(&[]),
            bounds,
            Vec::new()
        )
        .unwrap()
        .1,
        archive
    );
    for changed in [
        ContainerImageLimits {
            compressed_bytes: snapshot.layers()[0].bytes - 1,
            ..bounds
        },
        ContainerImageLimits {
            unpacked_bytes: rootfs.len() as u64 - 1,
            ..bounds
        },
    ] {
        let mut output = Vec::new();
        assert!(
            write_container_snapshot_image(
                &image,
                config,
                &spool(&rootfs),
                spool(&[]),
                changed,
                &mut output
            )
            .is_err()
        );
        assert!(output.is_empty());
    }
}

#[test]
fn snapshot_rejects_configuration_substitution_and_invalid_rootfs_before_publication() {
    let graph = Graph::ordinary();
    let image = graph.verify(limits()).unwrap();
    let config = &graph.blobs[&image.config().sha256];
    let root = tar(&[("state", tar::EntryType::Regular, None, b"saved")]);
    for changed in [Vec::new(), b"{}".to_vec(), {
        let mut config = config.clone();
        config[0] ^= 1;
        config
    }] {
        assert!(
            write_container_snapshot_image(
                &image,
                &changed,
                &spool(&root),
                spool(&[]),
                limits(),
                Vec::new()
            )
            .is_err()
        );
    }
    let mut corrupted = root.clone();
    corrupted[0] ^= 1;
    for invalid in [
        Vec::new(),
        root[..256].to_vec(),
        corrupted,
        tar(&[("state", tar::EntryType::Char, None, b"")]),
        tar(&[("state", tar::EntryType::Link, Some("../outside"), b"")]),
    ] {
        let mut output = Vec::new();
        assert!(
            write_container_snapshot_image(
                &image,
                config,
                &spool(&invalid),
                spool(&[]),
                limits(),
                &mut output
            )
            .is_err()
        );
        assert!(output.is_empty());
    }
    let (snapshot, archive) = write_container_snapshot_image(
        &image,
        config,
        &spool(&root),
        spool(&[]),
        limits(),
        Vec::new(),
    )
    .unwrap();
    let (_, snapshot_config) = read_container_image_configuration(
        &spool(&archive),
        &crate::ArtifactRef::new(1, &hex::encode(hash(&archive)), archive.len() as u64).unwrap(),
        *snapshot.manifest_sha256(),
        "arm64",
        limits(),
    )
    .unwrap();
    let json_limit = config.len().max(snapshot_config.len()) as u64;
    assert!(json_limit < snapshot.manifest_bytes);
    let mut output = Vec::new();
    assert!(
        write_container_snapshot_image(
            &image,
            config,
            &spool(&root),
            spool(&[]),
            ContainerImageLimits {
                json_bytes: json_limit,
                ..limits()
            },
            &mut output,
        )
        .is_err()
    );
    assert!(output.is_empty());
    for bounds in [
        ContainerImageLimits {
            json_bytes: 1,
            ..limits()
        },
        ContainerImageLimits {
            compressed_bytes: 1,
            ..limits()
        },
        ContainerImageLimits {
            unpacked_bytes: 1,
            ..limits()
        },
        ContainerImageLimits {
            layers: 0,
            ..limits()
        },
        ContainerImageLimits {
            entries: 0,
            ..limits()
        },
    ] {
        assert!(
            write_container_snapshot_image(
                &image,
                config,
                &spool(&root),
                spool(&[]),
                bounds,
                Vec::new()
            )
            .is_err()
        );
    }
    let root = spool(&tar(&[
        ("a", tar::EntryType::Regular, None, b"a"),
        ("b", tar::EntryType::Regular, None, b"b"),
    ]));
    assert!(
        write_container_snapshot_image(
            &image,
            config,
            &root,
            spool(&[]),
            ContainerImageLimits {
                entries: 1,
                ..limits()
            },
            Vec::new(),
        )
        .is_err()
    );
}

#[test]
fn configuration_read_requires_the_complete_sealed_source_artifact() {
    let (source, artifact, image) = sealed(&Graph::ordinary());
    for (artifact, expected, architecture) in [
        (
            crate::ArtifactRef::new(1, &"a".repeat(64), artifact.size()).unwrap(),
            *image.manifest_sha256(),
            "arm64",
        ),
        (artifact.clone(), [0; 32], "arm64"),
        (artifact.clone(), *image.manifest_sha256(), "amd64"),
        (artifact, *image.manifest_sha256(), "unsupported"),
    ] {
        assert!(
            read_container_image_configuration(
                &source,
                &artifact,
                expected,
                architecture,
                limits()
            )
            .is_err()
        );
    }
}

#[test]
fn snapshot_rehashes_the_spool_and_sanitizes_output_failures() {
    use std::os::unix::fs::FileExt;
    struct MutatingWriter(File);
    impl Write for MutatingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.write_at(b"X", 0)?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    struct Failed;
    impl Write for Failed {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("private-spool-path"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let graph = Graph::ordinary();
    let image = graph.verify(limits()).unwrap();
    let config = &graph.blobs[&image.config().sha256];
    let rootfs = tar(&[("state", tar::EntryType::Regular, None, b"saved")]);
    for mutate_compressed in [false, true] {
        let root = spool(&rootfs);
        let compressed = spool(&[]);
        let changed = if mutate_compressed {
            &compressed
        } else {
            &root
        };
        let output = MutatingWriter(changed.try_clone().unwrap());
        assert!(
            write_container_snapshot_image(&image, config, &root, compressed, limits(), output)
                .is_err()
        );
    }
    let error = write_container_snapshot_image(
        &image,
        config,
        &spool(&rootfs),
        spool(&[]),
        limits(),
        Failed,
    )
    .err()
    .unwrap();
    assert!(!error.to_string().contains("private-spool-path"));
}

#[test]
fn snapshot_rejects_nonempty_or_unwritable_compressed_spool_before_publication() {
    let graph = Graph::ordinary();
    let image = graph.verify(limits()).unwrap();
    let config = &graph.blobs[&image.config().sha256];
    let root = spool(&tar(&[("state", tar::EntryType::Regular, None, b"saved")]));
    let nonempty = spool(b"retained");
    let retained = nonempty.try_clone().unwrap();
    let scratch = tempfile::NamedTempFile::new().unwrap();
    let readonly = File::open(scratch.path()).unwrap();
    for compressed in [nonempty, readonly] {
        let mut output = Vec::new();
        assert!(
            write_container_snapshot_image(
                &image,
                config,
                &root,
                compressed,
                limits(),
                &mut output
            )
            .is_err()
        );
        assert!(output.is_empty());
    }
    let mut saved = Vec::new();
    restore::FileReader::new(&retained, 0)
        .unwrap()
        .read_to_end(&mut saved)
        .unwrap();
    assert_eq!(saved, b"retained");
}
