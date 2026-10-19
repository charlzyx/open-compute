//! Fork provenance and independent upstream type identity.

use super::*;

#[test]
fn lock_source_provenance_and_independent_types_are_validated() {
    let mut good: serde_json::Value =
        serde_json::from_str(&lock_json(&"ab".repeat(32), "")).unwrap();
    good["workersTypes"]["gitHead"] = serde_json::json!("12".repeat(20));
    let parsed = RuntimeLock::parse(&serde_json::to_vec(&good).unwrap()).unwrap();
    assert!(
        parsed
            .current_target()
            .unwrap()
            .1
            .archive_url
            .starts_with("https://github.com/")
    );
    assert_ne!(parsed.workers_types.git_head, parsed.revision);
    for (pointer, invalid) in [
        (
            "/source/repository",
            serde_json::json!("https://github.com/unrelated/workerd"),
        ),
        ("/source/upstreamBase", serde_json::json!("not-a-revision")),
        ("/source/buildInputs", serde_json::json!({})),
        ("/source/buildInputs/bazel", serde_json::json!("")),
        ("/source/buildInputs/bazel", serde_json::json!("9.2.0\n")),
        (
            "/source/buildInputs/bazel",
            serde_json::json!("x".repeat(2049)),
        ),
        ("/source/buildInputs/mode", serde_json::json!("fastbuild")),
        ("/source/buildInputs/strip", serde_json::json!("never")),
        (
            "/source/buildInputs/macosExecRustStrip",
            serde_json::json!("always"),
        ),
        (
            "/source/buildInputs/target",
            serde_json::json!("//unrelated:server"),
        ),
        ("/release", serde_json::json!("../untrusted")),
        ("/release", serde_json::json!("x".repeat(129))),
    ] {
        let mut value = good.clone();
        *value.pointer_mut(pointer).unwrap() = invalid;
        assert!(
            RuntimeLock::parse(&serde_json::to_vec(&value).unwrap()).is_err(),
            "{pointer}"
        );
    }
    let mut value = good.clone();
    value["targets"][host_target()]["archiveUrl"] = serde_json::Value::Null;
    assert!(RuntimeLock::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    for invalid_key in [String::new(), "x".repeat(129), "control\u{7f}".to_owned()] {
        let mut value = good.clone();
        value["source"]["buildInputs"][invalid_key] = serde_json::json!("value");
        assert!(RuntimeLock::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    let mut value = good.clone();
    for index in 0..65 {
        value["source"]["buildInputs"][format!("extra-{index}")] = serde_json::json!("value");
    }
    assert!(RuntimeLock::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    good.as_object_mut().unwrap().remove("source");
    assert!(RuntimeLock::parse(&serde_json::to_vec(&good).unwrap()).is_err());
}

#[test]
fn compatibility_artifact_provenance_is_verified_without_weakening_release_pins() {
    let bytes = include_bytes!("../../../packages/runtime/workerd-glibc228.lock.json");
    let good: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    let parsed = RuntimeLock::parse(bytes).unwrap();
    assert_eq!(parsed.targets.len(), 1);
    assert_eq!(
        parsed.source.compatibility_artifact_url().unwrap().unwrap(),
        parsed.targets["linux-x64"].archive_url
    );
    for (pointer, invalid) in [
        ("/source/buildInputs/compatibilityProfile", "unknown"),
        (
            "/source/buildInputs/compatibilityWorkflowRun",
            "http://github.com/owner/repo/actions/runs/1",
        ),
        (
            "/source/buildInputs/compatibilityWorkflowRun",
            "https://example.com/owner/repo/actions/runs/1",
        ),
        (
            "/source/buildInputs/compatibilityWorkflowRun",
            "https://github.com/owner/repo/actions/runs/1?token=secret",
        ),
        (
            "/source/buildInputs/compatibilityWorkflowRun",
            "https://github.com/owner/repo/actions/runs/0",
        ),
        ("/source/buildInputs/compatibilityArtifactId", "0"),
        ("/source/buildInputs/compatibilityArtifactId", "../other"),
        ("/source/buildInputs/compatibilityBuilderSha256", "invalid"),
        ("/source/buildInputs/compatibilityProducerCommit", "invalid"),
        (
            "/targets/linux-x64/archiveUrl",
            "https://github.com/owner/repo/actions/runs/1/artifacts/2",
        ),
        ("/release", "unqualified-release"),
    ] {
        let mut value = good.clone();
        *value.pointer_mut(pointer).unwrap() = serde_json::json!(invalid);
        assert!(
            RuntimeLock::parse(&serde_json::to_vec(&value).unwrap()).is_err(),
            "{pointer}"
        );
    }
    for key in [
        "compatibilityWorkflowRun",
        "compatibilityArtifactId",
        "compatibilityBuilderSha256",
        "compatibilityProducerCommit",
    ] {
        let mut value = good.clone();
        value["source"]["buildInputs"]
            .as_object_mut()
            .unwrap()
            .remove(key);
        assert!(
            RuntimeLock::parse(&serde_json::to_vec(&value).unwrap()).is_err(),
            "{key}"
        );
    }
    let mut multiple = good.clone();
    multiple["targets"]["linux-arm64"] = multiple["targets"]["linux-x64"].clone();
    assert!(RuntimeLock::parse(&serde_json::to_vec(&multiple).unwrap()).is_err());
    let mut unsigned = good.clone();
    unsigned["source"]["buildInputs"]
        .as_object_mut()
        .unwrap()
        .remove("compatibilityProfile");
    assert!(RuntimeLock::parse(&serde_json::to_vec(&unsigned).unwrap()).is_err());
}
