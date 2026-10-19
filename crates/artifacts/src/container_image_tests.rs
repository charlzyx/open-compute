use super::*;
use flate2::{Compression, write::GzEncoder};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Cursor, Write};

#[path = "container_image_archive_tests.rs"]
mod archive;

#[path = "container_image_snapshot_tests.rs"]
mod snapshot;

fn hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
fn token(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(hash(bytes)))
}
fn limits() -> ContainerImageLimits {
    ContainerImageLimits {
        json_bytes: 8192,
        compressed_bytes: 65536,
        unpacked_bytes: 65536,
        layers: 8,
        entries: 8,
    }
}
fn tar(entries: &[(&str, tar::EntryType, Option<&str>, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, kind, target, bytes) in entries {
        let mut header = tar::Header::new_ustar();
        header.set_mode(0o755);
        header.set_size(bytes.len() as u64);
        header.set_entry_type(*kind);
        if let Some(target) = target {
            header.set_link_name(target).unwrap();
        }
        builder.append_data(&mut header, name, *bytes).unwrap();
    }
    builder.into_inner().unwrap()
}
fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut compressor = GzEncoder::new(Vec::new(), Compression::fast());
    compressor.write_all(bytes).unwrap();
    compressor.finish().unwrap()
}
struct Graph {
    manifest: Value,
    blobs: BTreeMap<[u8; 32], Vec<u8>>,
}
impl Graph {
    fn new(layers: &[(Vec<u8>, bool)]) -> Self {
        let config = serde_json::to_vec(&json!({ "architecture":"arm64", "os":"linux",
            "rootfs":{"type":"layers","diff_ids":layers.iter().map(|(tar,_)| token(tar)).collect::<Vec<_>>()},
            "config":{"Env":["HELLO=world"],"Cmd":["/bin/sh"]} })).unwrap();
        let mut blobs = BTreeMap::from([(hash(&config), config.clone())]);
        let descriptors = layers.iter().map(|(tar, compressed)| {
            let blob = if *compressed { gzip(tar) } else { tar.clone() };
            let descriptor = json!({ "mediaType":if *compressed {"application/vnd.oci.image.layer.v1.tar+gzip"} else {"application/vnd.oci.image.layer.v1.tar"},
                "digest":token(&blob), "size":blob.len() });
            blobs.insert(hash(&blob), blob);
            descriptor
        }).collect::<Vec<_>>();
        Self {
            manifest: json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json",
            "config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":token(&config),"size":config.len()},
            "layers":descriptors}),
            blobs,
        }
    }
    fn ordinary() -> Self {
        Self::new(&[(
            tar(&[("bin/app", tar::EntryType::Regular, None, b"hello")]),
            true,
        )])
    }
    fn verify(
        &self,
        bounds: ContainerImageLimits,
    ) -> Result<VerifiedContainerImage, PlatformError> {
        let bytes = serde_json::to_vec(&self.manifest).unwrap();
        verify_container_image(&bytes, hash(&bytes), "arm64", bounds, |blob| {
            self.blobs
                .get(&blob.sha256)
                .cloned()
                .map(Cursor::new)
                .ok_or_else(integrity_error)
        })
    }
    fn edit_config(&mut self, edit: impl FnOnce(&mut Value)) {
        let descriptor = &mut self.manifest["config"];
        let old = digest(descriptor["digest"].as_str().unwrap()).unwrap();
        let mut config: Value = serde_json::from_slice(&self.blobs[&old]).unwrap();
        edit(&mut config);
        let bytes = serde_json::to_vec(&config).unwrap();
        descriptor["digest"] = json!(token(&bytes));
        descriptor["size"] = json!(bytes.len());
        self.blobs.insert(hash(&bytes), bytes);
    }
}

#[test]
fn complete_graph_checks_gzip_and_plain_tar_without_extracting_paths() {
    let plain = tar(&[("app", tar::EntryType::Regular, None, b"main")]);
    let links = tar(&[
        ("bin/", tar::EntryType::Directory, None, b""),
        ("bin/app", tar::EntryType::Regular, None, b"executable"),
        ("bin/hard", tar::EntryType::Link, Some("bin/app"), b""),
        ("bin/link", tar::EntryType::Symlink, Some("/bin/app"), b""),
    ]);
    let graph = Graph::new(&[(plain.clone(), false), (links.clone(), true)]);
    let image = graph.verify(limits()).unwrap();
    assert_eq!(image.architecture(), "arm64");
    assert_eq!(image.layers().len(), 2);
    assert_eq!(image.unpacked_bytes(), (plain.len() + links.len()) as u64);
    assert_eq!(
        image.manifest_sha256(),
        &hash(&serde_json::to_vec(&graph.manifest).unwrap())
    );
    assert_eq!(
        image.config().sha256,
        digest(graph.manifest["config"]["digest"].as_str().unwrap()).unwrap()
    );
    let mut docker = Graph::ordinary();
    docker.manifest["mediaType"] = json!("application/vnd.docker.distribution.manifest.v2+json");
    docker.manifest["config"]["mediaType"] =
        json!("application/vnd.docker.container.image.v1+json");
    docker.manifest["layers"][0]["mediaType"] =
        json!("application/vnd.docker.image.rootfs.diff.tar.gzip");
    docker.verify(limits()).unwrap();
}

#[test]
fn graph_bounds_apply_before_and_during_blob_reads() {
    let graph = Graph::ordinary();
    let configs = [
        ContainerImageLimits {
            json_bytes: 0,
            ..limits()
        },
        ContainerImageLimits {
            compressed_bytes: 0,
            ..limits()
        },
        ContainerImageLimits {
            unpacked_bytes: 0,
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
        ContainerImageLimits {
            json_bytes: 10,
            ..limits()
        },
        ContainerImageLimits {
            compressed_bytes: 1,
            ..limits()
        },
        ContainerImageLimits {
            unpacked_bytes: 512,
            ..limits()
        },
    ];
    for bound in configs {
        assert!(graph.verify(bound).is_err());
    }
    let two = Graph::new(&[
        (tar(&[("a", tar::EntryType::Regular, None, b"a")]), true),
        (tar(&[("b", tar::EntryType::Regular, None, b"b")]), true),
    ]);
    assert!(
        two.verify(ContainerImageLimits {
            layers: 1,
            ..limits()
        })
        .is_err()
    );
    assert!(
        two.verify(ContainerImageLimits {
            entries: 1,
            ..limits()
        })
        .is_err()
    );
    let first_size = two.manifest["layers"][0]["size"].as_u64().unwrap();
    assert!(
        two.verify(ContainerImageLimits {
            compressed_bytes: first_size + 1,
            ..limits()
        })
        .is_err()
    );
    assert!(
        two.verify(ContainerImageLimits {
            unpacked_bytes: 3072,
            ..limits()
        })
        .is_err()
    );
}

#[test]
fn manifest_and_config_cannot_redirect_or_change_platform() {
    for (path, value) in [
        ("schemaVersion", json!(1)),
        (
            "mediaType",
            json!("application/vnd.oci.image.index.v1+json"),
        ),
        ("artifactType", json!("any")),
        ("subject", json!({"digest":"any"})),
    ] {
        let mut graph = Graph::ordinary();
        graph.manifest[path] = value;
        assert!(graph.verify(limits()).is_err());
    }
    for (part, field, value) in [
        ("config", "mediaType", json!("arbitrary")),
        ("config", "size", json!(0)),
        ("config", "size", json!(9000)),
        (
            "config",
            "urls",
            json!(["https://registry.invalid/private"]),
        ),
        ("config", "data", json!("embedded")),
        ("config", "digest", json!("sha256:UPPERCASE")),
    ] {
        let mut graph = Graph::ordinary();
        graph.manifest[part][field] = value;
        assert!(graph.verify(limits()).is_err());
    }
    for (field, value) in [
        ("architecture", json!("amd64")),
        ("os", json!("windows")),
        ("rootfs", json!({"type":"other","diff_ids":[]})),
    ] {
        let mut graph = Graph::ordinary();
        graph.edit_config(|config| config[field] = value);
        assert!(graph.verify(limits()).is_err());
    }
    let mut graph = Graph::ordinary();
    graph.edit_config(|config| config["rootfs"]["diff_ids"] = json!([]));
    assert!(graph.verify(limits()).is_err());
    let mut graph = Graph::ordinary();
    graph.manifest["layers"][0]["mediaType"] = json!("application/vnd.oci.image.layer.v1.tar+zstd");
    assert!(graph.verify(limits()).is_err());
    let bytes = serde_json::to_vec(&graph.manifest).unwrap();
    for arch in ["x86_64", "arm64"] {
        assert!(
            verify_container_image(&bytes, [0; 32], arch, limits(), |_| Ok(Cursor::new(Vec::<
                u8,
            >::new(
            ))))
            .is_err()
        );
    }
    for bytes in [
        b"".as_slice(),
        b"{",
        b"{\"schemaVersion\":2,\"schemaVersion\":2}",
    ] {
        assert!(
            verify_container_image(bytes, hash(bytes), "arm64", limits(), |_| Ok(Cursor::new(
                Vec::<u8>::new()
            )))
            .is_err()
        );
    }
    let mut graph = Graph::ordinary();
    graph.edit_config(|config| {
        config["rootfs"]["diff_ids"][0] = json!(format!("sha256:{}", "0".repeat(64)));
    });
    assert!(graph.verify(limits()).is_err());
    let mut graph = Graph::ordinary();
    graph.edit_config(|config| config["rootfs"]["diff_ids"][0] = json!("sha512:anything"));
    assert!(graph.verify(limits()).is_err());
}

#[test]
fn every_compressed_and_uncompressed_digest_is_required() {
    for change in 0..4 {
        let mut graph = Graph::ordinary();
        let key = digest(graph.manifest["config"]["digest"].as_str().unwrap()).unwrap();
        match change {
            0 => {
                graph.blobs.remove(&key);
            }
            1 => graph.blobs.get_mut(&key).unwrap()[0] ^= 1,
            2 => {
                graph.blobs.get_mut(&key).unwrap().pop();
            }
            _ => graph.blobs.get_mut(&key).unwrap().push(0),
        }
        assert!(graph.verify(limits()).is_err());
    }
    let graph = Graph::ordinary();
    let bytes = serde_json::to_vec(&graph.manifest).unwrap();
    assert!(
        verify_container_image(&bytes, hash(&bytes), "arm64", limits(), |_| Err::<
            Cursor<Vec<u8>>,
            _,
        >(
            integrity_error()
        ))
        .is_err()
    );
    for change in 0..4 {
        let mut graph = Graph::ordinary();
        let key = digest(graph.manifest["layers"][0]["digest"].as_str().unwrap()).unwrap();
        match change {
            0 => graph.blobs.get_mut(&key).unwrap()[0] ^= 1,
            1 => {
                graph.blobs.get_mut(&key).unwrap().pop();
            }
            2 => graph.manifest["layers"][0]["size"] = json!(graph.blobs[&key].len() + 1),
            _ => {
                graph.blobs.remove(&key);
            }
        }
        assert!(graph.verify(limits()).is_err());
    }
}

#[test]
fn archive_paths_types_links_duplicates_and_trailing_archives_fail_closed() {
    let cases = [
        tar(&[
            ("x", tar::EntryType::Regular, None, b"a"),
            ("x", tar::EntryType::Regular, None, b"b"),
        ]),
        tar(&[("x", tar::EntryType::Char, None, b"")]),
        tar(&[("x", tar::EntryType::Link, Some("../outside"), b"")]),
        tar(&[("x", tar::EntryType::Symlink, None, b"")]),
    ];
    for bytes in cases {
        assert!(Graph::new(&[(bytes, false)]).verify(limits()).is_err());
    }
    for path in [b"../outside".as_slice(), b"/outside", b"x/../../outside"] {
        let mut bytes = tar(&[("x", tar::EntryType::Regular, None, b"a")]);
        let mut header = tar::Header::from_byte_slice(&bytes[..512]).clone();
        header.as_mut_bytes()[..100].fill(0);
        header.as_mut_bytes()[..path.len()].copy_from_slice(path);
        header.set_cksum();
        bytes[..512].copy_from_slice(header.as_bytes());
        assert!(Graph::new(&[(bytes, true)]).verify(limits()).is_err());
    }
    let mut bytes = tar(&[("x", tar::EntryType::Regular, None, b"a")]);
    bytes.extend(tar(&[("hidden", tar::EntryType::Regular, None, b"b")]));
    assert!(Graph::new(&[(bytes, true)]).verify(limits()).is_err());
    let mut bytes = tar(&[("x", tar::EntryType::Regular, None, b"a")]);
    bytes[0] ^= 1;
    assert!(Graph::new(&[(bytes, false)]).verify(limits()).is_err());
    assert!(Graph::new(&[(Vec::new(), false)]).verify(limits()).is_err());
    for path in [
        b"".as_slice(),
        b"/absolute",
        b"../parent",
        b"a\0b",
        &[b'x'; 4097],
    ] {
        assert!(!valid_path(path));
    }
}

#[test]
fn checked_reader_sanitizes_io_failures_and_rejects_surplus_bytes() {
    let mut reader = CheckedReader::new(Cursor::new(b"1234"), 3);
    assert!(reader.read_to_end(&mut Vec::new()).is_err());
    let mut reader = CheckedReader::new(Cursor::new(b"123"), 3);
    reader.read_to_end(&mut Vec::new()).unwrap();
    reader.verify(&hash(b"123")).unwrap();
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("private provider details"))
        }
    }
    let graph = Graph::ordinary();
    let bytes = serde_json::to_vec(&graph.manifest).unwrap();
    let error = verify_container_image(&bytes, hash(&bytes), "arm64", limits(), |_| Ok(Broken))
        .unwrap_err();
    assert!(!error.message().contains("private"));
}

#[test]
fn archive_extensions_are_bounded_and_resolved_before_path_checks() {
    let long_name = format!("dir/{}/app", "x".repeat(300));
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o755);
    header.set_size(3);
    builder
        .append_data(&mut header, &long_name, b"app".as_slice())
        .unwrap();
    let graph = Graph::new(&[(builder.into_inner().unwrap(), true)]);
    graph.verify(limits()).unwrap();
    assert!(
        graph
            .verify(ContainerImageLimits {
                entries: 1,
                ..limits()
            })
            .is_err()
    );

    for name in ["app", "../outside"] {
        let mut builder = tar::Builder::new(Vec::new());
        builder
            .append_pax_extensions([("path", name.as_bytes())])
            .unwrap();
        let mut header = tar::Header::new_ustar();
        header.set_mode(0o755);
        header.set_size(3);
        builder
            .append_data(&mut header, "safe", b"app".as_slice())
            .unwrap();
        let result = Graph::new(&[(builder.into_inner().unwrap(), false)]).verify(limits());
        assert_eq!(result.is_ok(), name == "app");
    }
    let alias = tar(&[
        ("./app", tar::EntryType::Regular, None, b"a"),
        ("app", tar::EntryType::Regular, None, b"b"),
    ]);
    assert!(Graph::new(&[(alias, false)]).verify(limits()).is_err());
    let oversized = tar(&[("metadata", tar::EntryType::XHeader, None, &[b'x'; 16385])]);
    assert!(Graph::new(&[(oversized, false)]).verify(limits()).is_err());
    let truncated = tar(&[("app", tar::EntryType::Regular, None, b"app")])[..256].to_vec();
    assert!(Graph::new(&[(truncated, false)]).verify(limits()).is_err());
    let mut reader = ArchiveReader::new(Cursor::new(Vec::<u8>::new()), 1);
    assert_eq!(reader.read(&mut []).unwrap(), 0);
    assert_eq!(reader.read(&mut [0; 512]).unwrap(), 0);
}
