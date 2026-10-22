//! Real exec streams and code-only deployment retain their original process authority.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_exec_streams_and_code_updates_preserve_owned_processes() {
    let inputs = inputs();
    let provider = QualifiedContainerProvider::preflight(&inputs.containers)
        .await
        .unwrap();
    provider.ensure_active().await.unwrap();
    let purpose = repo().join(".temp/p23-product");
    fs::create_dir_all(&purpose).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("exec-options-")
        .tempdir_in(&purpose)
        .unwrap();
    let root = directory.path().to_owned();
    let _evidence = platform_process::Evidence(Some(directory));
    let data = root.join("data");
    let storage = PlatformStorage::bootstrap(
        &DataConfig {
            path: data.clone(),
            master_key_file: data.join("keys/master.key"),
            ..Default::default()
        },
        &SystemClock,
    )
    .unwrap();
    let instance = storage.identity().instance_id;
    drop(storage);
    let _cleanup = cleanup::Cleanup {
        data: data.clone(),
        instance,
        provider: inputs.containers.clone(),
    };
    let mock = MockS3::spawn("open-compute").await;
    let (public, admin) = platform_process::distinct_addresses();
    let config = platform_process::config(&root, &data, &mock.endpoint, public, admin);
    let mut text = fs::read_to_string(&config).unwrap();
    text.push('\n');
    text.push_str(
        &toml::to_string(&ContainerConfig {
            containers: &inputs.containers,
            cache: CacheConfig {
                max_bytes: 1 << 30,
                max_artifact_bytes: 1 << 30,
                ..Default::default()
            },
        })
        .unwrap(),
    );
    PlatformConfig::from_toml_str(&text).unwrap();
    fs::write(&config, text).unwrap();
    let log = root.join("ocd.stderr.log");
    let mut process = platform_process::spawn(&config, &log);
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http();
    platform_process::ready(&client, admin, &mut process).await;
    let admin = admin.to_string();
    let public = public.to_string();
    let accounts = http::json(
        &client,
        &admin,
        &admin,
        "/client/v4/accounts",
        "GET",
        Some("Bearer workflow-read-only"),
        Value::Null,
    )
    .await;
    let account = accounts["result"][0]["id"].as_str().unwrap();
    let image = inputs.images[0].push(&client, &admin, account).await;
    let alternate = inputs.images[1].push(&client, &admin, account).await;
    assert_ne!(image, alternate);
    let project = cf::project(&root);
    cf::deploy(
        &project,
        &root.join("cf-state"),
        &admin,
        account,
        "one",
        &image,
        [&image, &alternate],
    )
    .await;
    let host = format!("containers-fixture.{instance}.localhost");
    exec_options::lifecycle(&client, &public, &host).await;
    named_images::assert_stopped(
        &data,
        instance,
        named_images::application(&data, instance),
        &provider,
    )
    .await;
    let first = tenant(&client, &public, &host, "/").await;
    assert_eq!(first["code"], "one");
    assert_eq!(first["image"], "one");
    assert_eq!(first["visits"], 1);
    let application = {
        let db = ControlDb::open_readonly_wal_aware(&data.join("control.sqlite"), 1000).unwrap();
        ContainerAuthority::new(&db)
            .applications(instance)
            .unwrap()
            .into_iter()
            .find(|app| app.name == "containers-fixture")
            .unwrap()
            .id
    };
    let original = wait_running(&data, instance, application, 1).await;
    let proof = provider.observe(&original.opaque_engine_key).await.unwrap();
    assert!(proof.running && !proof.terminated);
    cf::update_code(&project);
    cf::deploy(
        &project,
        &root.join("cf-state"),
        &admin,
        account,
        "two",
        &image,
        [&image, &alternate],
    )
    .await;
    let updated = tenant(&client, &public, &host, "/").await;
    assert_eq!(updated["code"], "two");
    assert_eq!(updated["image"], "one");
    assert_eq!(updated["visits"], 2);
    let retained = wait_running(&data, instance, application, 1).await;
    assert_eq!(retained.identity, original.identity);
    assert_eq!(retained.start_identity, original.start_identity);
    let after = provider.observe(&original.opaque_engine_key).await.unwrap();
    assert!(after.running && !after.terminated);
    assert_eq!(after.start_identity, proof.start_identity);
    process.stop().await;
}
