//! Original pinned cf deploy producer against the owned local daemon.

use open_compute_core::{InstanceId, VersionId};
use open_compute_storage::{control_db::ControlDb, worker_repository::WorkerRepository};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn copy_output(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let next = destination.join(entry.file_name());
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            copy_output(&entry.path(), &next);
        } else {
            assert!(
                kind.is_file(),
                "official Build Output must not contain symlinks"
            );
            fs::copy(entry.path(), next).unwrap();
        }
    }
}

pub(super) fn project(root: &Path) -> PathBuf {
    let repository = super::repo();
    let fixture = repository.join("test/applications/containers");
    let project = root.join("cf-project");
    copy_output(&fixture.join(".cloudflare"), &project.join(".cloudflare"));
    for file in ["cloudflare.config.ts", "build-input.ts"] {
        fs::copy(fixture.join(file), project.join(file)).unwrap();
    }
    std::os::unix::fs::symlink(
        repository.join("node_modules"),
        project.join("node_modules"),
    )
    .unwrap();
    project
}

pub(super) fn update_code(project: &Path) {
    let module = project.join(".cloudflare/output/v0/workers/default/bundle/index.js");
    let mut source = fs::read(&module).unwrap();
    source.extend_from_slice(b"\n// code-only revision\n");
    fs::write(module, source).unwrap();
}

pub(super) async fn deploy(
    project: &Path,
    state: &Path,
    address: &str,
    account: &str,
    revision: &str,
    image: &str,
    named_images: [&str; 2],
) {
    // Change only test-owned Build Output inputs, never the maintained generated bundle.
    let worker_path = project.join(".cloudflare/output/v0/workers/default/worker.config.json");
    let mut worker: Value = serde_json::from_slice(&fs::read(&worker_path).unwrap()).unwrap();
    assert_eq!(worker["name"], "containers-fixture");
    assert_eq!(worker["env"]["CODE_REVISION"]["type"], "text");
    worker["env"]["CODE_REVISION"]["value"] = Value::String(revision.to_owned());
    fs::write(worker_path, serde_json::to_vec(&worker).unwrap()).unwrap();
    for name in ["containers-fixture", "containers-idle-fixture"] {
        let container_path = project.join(format!(
            ".cloudflare/output/v0/containers/{name}/container.config.json"
        ));
        let mut container: Value =
            serde_json::from_slice(&fs::read(&container_path).unwrap()).unwrap();
        assert_eq!(container["name"], name);
        assert_eq!(container["instanceType"], "lite");
        assert!(container["image"]["reference"].is_string());
        container["image"]["reference"] = Value::String(image.to_owned());
        fs::write(container_path, serde_json::to_vec(&container).unwrap()).unwrap();
    }
    let named_path = project
        .join(".cloudflare/output/v0/containers/containers-named-fixture/container.config.json");
    let mut named: Value = serde_json::from_slice(&fs::read(&named_path).unwrap()).unwrap();
    assert_eq!(named["name"], "containers-named-fixture");
    assert_eq!(named["schedulingPolicy"], "durable-object");
    for key in ["image", "instanceType", "maxInstances"] {
        assert!(named.get(key).is_none());
    }
    assert_eq!(named["images"].as_object().unwrap().len(), 2);
    for (alias, reference) in ["base", "alternate"].into_iter().zip(named_images) {
        assert!(named["images"][alias]["reference"].is_string());
        named["images"][alias]["reference"] = Value::String(reference.to_owned());
    }
    fs::write(named_path, serde_json::to_vec(&named).unwrap()).unwrap();
    super::http::cf(
        project,
        state,
        address,
        account,
        &[
            "deploy",
            "--prebuilt",
            "--mode",
            "production",
            "--containers-rollout",
            "immediate",
        ],
    )
    .await;
}

pub(super) fn active_version(data: &Path, instance: InstanceId) -> VersionId {
    let db = ControlDb::open_readonly_wal_aware(&data.join("control.sqlite"), 1000).unwrap();
    WorkerRepository::new(&db)
        .list_workers(instance)
        .unwrap()
        .into_iter()
        .find(|worker| worker.name == "containers-fixture")
        .unwrap()
        .active_version_id
        .unwrap()
}
