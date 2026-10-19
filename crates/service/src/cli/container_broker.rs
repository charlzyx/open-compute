//! Explicit foreground Broker command, independent of daemon scope management.

use super::*;
use std::io::Read;

pub(super) async fn run(path: &Path, cwd: &Path) -> Result<(), PlatformError> {
    let path = crate::config_load::lexical_absolute(cwd, path)?;
    let file = std::fs::File::open(path).map_err(|_| invalid())?;
    if !file.metadata().map_err(|_| invalid())?.is_file() {
        return Err(invalid());
    }
    let mut contents = String::new();
    file.take(65_537)
        .read_to_string(&mut contents)
        .map_err(|_| invalid())?;
    if contents.len() > 65_536 {
        return Err(invalid());
    }
    let config = toml::from_str(&contents).map_err(|_| invalid())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|_| invalid())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(|_| invalid())?;
    crate::container_broker::run_broker(config, async move {
        tokio::select! {
            _ = terminate.recv() => {},
            _ = interrupt.recv() => {},
        }
    })
    .await
}

fn invalid() -> PlatformError {
    PlatformError::new(
        ErrorCode::ConfigInvalid,
        "Container Broker configuration is invalid or unavailable",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broker_command_is_explicit_and_has_no_daemon_scope_dependencies() {
        let cli =
            parse_from(["ocd", "container-broker", "--broker-config", "broker.toml"]).unwrap();
        assert!(!operator_deps_required(&cli));
        assert!(matches!(cli.command, Command::ContainerBroker { .. }));
        for option in ["--system", "--config", "--instance"] {
            let mut args = vec![
                "ocd",
                "container-broker",
                "--broker-config",
                "broker.toml",
                option,
            ];
            if option != "--system" {
                args.push("chosen");
            }
            assert!(parse_from(args).is_err());
        }
        assert!(parse_from(["ocd", "container-broker"]).is_err());
    }

    #[tokio::test]
    async fn invalid_broker_input_is_bounded_and_does_not_start_an_engine() {
        let directory = tempfile::tempdir().unwrap();
        for contents in ["unknown = true".to_owned(), "x".repeat(65_537)] {
            let path = directory.path().join("private-input.toml");
            std::fs::write(&path, contents).unwrap();
            let error = run(&path, directory.path()).await.unwrap_err();
            assert_eq!(error.code(), ErrorCode::ConfigInvalid);
            assert!(!error.to_string().contains("private-input"));
        }
        assert!(run(directory.path(), directory.path()).await.is_err());
        assert!(
            run(&directory.path().join("missing"), directory.path())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn broker_cli_rejects_private_inputs_without_connecting_or_creating_authority() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let engine_path = directory.path().join("engine.sock");
        let engine = tokio::net::UnixListener::bind(&engine_path).unwrap();
        let state = directory.path().join("authority");
        let spool = directory.path().join("spool");
        let socket = directory.path().join("broker.sock");
        let secret = "private-broker-input-secret";
        let missing = directory.path().join(format!("{secret}-missing.toml"));
        let invalid_file = directory.path().join(format!("{secret}.toml"));
        let configuration = format!(
            "# {secret}\nsocket = {socket:?}\nengine_socket = {engine_path:?}\npeer_uid = {}\nqualification_image = \"sha256:{}\"\nexec_helper_volume = \"open-compute-exec-test\"\nexec_helper_sha256 = \"{}\"\nmax_leases = 4\nmax_instances = 2\nmax_requests = 8\nrelay_host = \"127.0.0.1\"\nspool_dir = {spool:?}\nstate_dir = {state:?}\nmax_image_bytes = 4096\nmax_stream_bytes = 4096\n[resources]\ncpuNanos = 62500000\nmemoryBytes = 268435456\ndiskBytes = 2000000000\npids = 32\nopenFiles = 64\n",
            rustix::process::getuid().as_raw(),
            "a".repeat(64),
            "1".repeat(64),
        );
        let mut cases = vec![
            (missing, None),
            (directory.path().to_path_buf(), None),
            (
                invalid_file.clone(),
                Some(format!("socket = \"{secret}\n").into_bytes()),
            ),
            (invalid_file.clone(), Some(vec![0xff])),
            (invalid_file.clone(), Some(vec![b'x'; 65_537])),
            (
                invalid_file.clone(),
                Some(format!("unknown = \"{secret}\"\n").into_bytes()),
            ),
        ];
        // Both policies parse as the closed Broker DTO but must fail before journal or engine I/O.
        for contents in [
            configuration.replace("max_instances = 2", "max_instances = 0"),
            configuration.replace(
                &format!("peer_uid = {}", rustix::process::getuid().as_raw()),
                &format!("peer_uid = {}", rustix::process::getuid().as_raw() ^ 1),
            ),
        ] {
            let _: crate::container_broker::BrokerConfig = toml::from_str(&contents).unwrap();
            cases.push((invalid_file.clone(), Some(contents.into_bytes())));
        }
        for (path, contents) in cases {
            if let Some(contents) = contents {
                std::fs::write(&path, contents).unwrap();
            }
            let cli = parse_from([
                "ocd",
                "--no-update-check",
                "container-broker",
                "--broker-config",
                path.to_str().unwrap(),
            ])
            .unwrap();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let exit = tokio::time::timeout(
                Duration::from_secs(2),
                execute(cli, &mut stdout, &mut stderr),
            )
            .await
            .unwrap();
            assert_eq!(exit, ExitCode::from(ExitClass::Config));
            assert!(stdout.is_empty());
            let stderr = String::from_utf8(stderr).unwrap();
            assert!(stderr.starts_with("CONFIG_INVALID:"));
            assert!(!stderr.contains(secret));
            assert!(!stderr.contains(directory.path().to_str().unwrap()));
            assert!(!state.exists());
            assert!(!spool.exists());
            assert!(!socket.exists());
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(25), engine.accept())
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
    }
}
