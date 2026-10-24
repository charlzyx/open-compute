use super::*;
use std::io::{Seek as _, SeekFrom};

#[test]
fn directory_archive_preserves_exact_tar_and_caller_cursor_for_raw_and_gzip_layers() {
    let expected = tar(&[
        ("data/", tar::EntryType::Directory, None, b""),
        ("data/value", tar::EntryType::Regular, None, b"saved"),
        ("data/relative", tar::EntryType::Symlink, Some("value"), b""),
        (
            "data/external",
            tar::EntryType::Symlink,
            Some("/elsewhere"),
            b"",
        ),
        (
            "data/parent",
            tar::EntryType::Symlink,
            Some("../../elsewhere"),
            b"",
        ),
        ("data/same", tar::EntryType::Link, Some("data/value"), b""),
    ]);
    for gzip in [false, true] {
        let graph = Graph::new(&[(expected.clone(), gzip)]);
        let (mut file, artifact, image) = snapshot::sealed(&graph);
        file.seek(SeekFrom::Start(7)).unwrap();
        let actual = write_container_directory_archive(
            &file,
            &artifact,
            *image.manifest_sha256(),
            "arm64",
            limits(),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(file.stream_position().unwrap(), 7);
        assert_eq!(hash(&actual), image.diff_ids()[0]);
        let inspected =
            verify_container_archive(actual.as_slice(), actual.len() as u64, 6).unwrap();
        assert_eq!(inspected.bytes, expected.len() as u64);
    }
}

#[test]
fn directory_archive_rejects_layers_substitution_corruption_and_bounds_before_use() {
    let layer = tar(&[("state", tar::EntryType::Regular, None, b"saved")]);
    for layers in [vec![], vec![(layer.clone(), true), (layer.clone(), false)]] {
        let graph = Graph::new(&layers);
        let (file, artifact, image) = snapshot::sealed(&graph);
        assert!(
            write_container_directory_archive(
                &file,
                &artifact,
                *image.manifest_sha256(),
                "arm64",
                limits(),
                Vec::new(),
            )
            .is_err()
        );
    }
    let graph = Graph::new(&[(layer, true)]);
    let (mut file, artifact, image) = snapshot::sealed(&graph);
    for (manifest, architecture, bounds) in [
        ([0; 32], "arm64", limits()),
        (*image.manifest_sha256(), "amd64", limits()),
        (
            *image.manifest_sha256(),
            "arm64",
            ContainerImageLimits {
                unpacked_bytes: 1,
                ..limits()
            },
        ),
        (
            *image.manifest_sha256(),
            "arm64",
            ContainerImageLimits {
                compressed_bytes: 1,
                ..limits()
            },
        ),
        (
            *image.manifest_sha256(),
            "arm64",
            ContainerImageLimits {
                entries: 0,
                ..limits()
            },
        ),
    ] {
        assert!(
            write_container_directory_archive(
                &file,
                &artifact,
                manifest,
                architecture,
                bounds,
                Vec::new()
            )
            .is_err()
        );
    }
    let wrong_artifact = crate::ArtifactRef::new(1, &"0".repeat(64), artifact.size()).unwrap();
    assert!(
        write_container_directory_archive(
            &file,
            &wrong_artifact,
            *image.manifest_sha256(),
            "arm64",
            limits(),
            Vec::new()
        )
        .is_err()
    );
    file.seek(SeekFrom::Start(600)).unwrap();
    file.write_all(b"changed").unwrap();
    assert!(
        write_container_directory_archive(
            &file,
            &artifact,
            *image.manifest_sha256(),
            "arm64",
            limits(),
            Vec::new()
        )
        .is_err()
    );
}

#[test]
fn directory_archive_sanitizes_failed_output_and_never_returns_partial_success() {
    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("private-directory-spool"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let graph = Graph::ordinary();
    let (file, artifact, image) = snapshot::sealed(&graph);
    let error = write_container_directory_archive(
        &file,
        &artifact,
        *image.manifest_sha256(),
        "arm64",
        limits(),
        FailingWriter,
    )
    .err()
    .unwrap();
    assert!(!error.to_string().contains("private-directory-spool"));
}

#[test]
fn directory_archive_rejects_writes_beneath_links_in_any_header_order() {
    let cases = [
        vec![
            (".", tar::EntryType::Symlink, Some("../10"), b"".as_slice()),
            (
                "escaped",
                tar::EntryType::Regular,
                None,
                b"escape".as_slice(),
            ),
        ],
        vec![
            (
                "pivot",
                tar::EntryType::Symlink,
                Some("../10"),
                b"".as_slice(),
            ),
            (
                "pivot/escaped",
                tar::EntryType::Regular,
                None,
                b"escape".as_slice(),
            ),
        ],
        vec![
            (
                "pivot/escaped",
                tar::EntryType::Regular,
                None,
                b"escape".as_slice(),
            ),
            (
                "pivot",
                tar::EntryType::Symlink,
                Some("../10"),
                b"".as_slice(),
            ),
        ],
        vec![
            (
                "pivot",
                tar::EntryType::Symlink,
                Some("/open-compute-directories/10"),
                b"".as_slice(),
            ),
            ("alias", tar::EntryType::Link, Some("pivot"), b"".as_slice()),
            (
                "alias/escaped",
                tar::EntryType::Regular,
                None,
                b"escape".as_slice(),
            ),
        ],
        vec![
            (
                "pivot",
                tar::EntryType::Symlink,
                Some("../10"),
                b"".as_slice(),
            ),
            (
                "alias",
                tar::EntryType::Link,
                Some("pivot/sentinel"),
                b"".as_slice(),
            ),
        ],
    ];
    for entries in cases {
        let layer = tar(&entries);
        verify_container_archive(layer.as_slice(), layer.len() as u64, 16).unwrap();
        let graph = Graph::new(&[(layer, true)]);
        let (file, artifact, image) = snapshot::sealed(&graph);
        assert!(
            write_container_directory_archive(
                &file,
                &artifact,
                *image.manifest_sha256(),
                "arm64",
                limits(),
                Vec::new()
            )
            .is_err()
        );
    }
}
